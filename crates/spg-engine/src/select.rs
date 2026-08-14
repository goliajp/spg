//! SELECT execution — the window / meta-view / CTE variants and the
//! subquery-resolution pre-pass. Lifted out of `lib.rs` (v7.32 engine
//! modularisation). These `impl Engine` methods are dispatched from the
//! bare-SELECT entry points and drive the non-trivial SELECT shapes.

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{
    ColumnName, Expr, FromClause, SelectItem, SelectStatement, Statement, TableRef, UnionKind,
};
use spg_storage::{
    Catalog, ColumnSchema, DataType, Row, StorageError, TableSchema, Value, VecEncoding,
};

use crate::describe;
use crate::eval::{EvalContext, EvalError};
use crate::join::RowRef;
use crate::system_catalog::collect_view_refs;
use crate::{
    ByteBudget, CancelToken, Engine, EngineError, OrderKey, QueryResult, aggregate,
    apply_offset_and_limit, apply_offset_and_limit_tagged, approx_row_bytes, build_order_keys,
    collect_meta_view_names, collect_qualified_refs, collect_scalar_subqueries,
    collect_window_nodes, compute_window_partition, eval, expr_tree_has_subquery,
    materialise_in_order, materialise_meta_view, memoize, order_by_value_cmp_in, partition_key_cmp,
    rewrite_window_to_columns, select_has_window, select_references_meta_view, select_refers_to,
    sort_by_keys, synth_info_key_column_usage, synth_info_referential_constraints,
    synth_info_routines, synth_info_statistics, synth_information_schema_columns,
    synth_information_schema_tables, synth_mysql_db, synth_mysql_user, synth_pg_attribute,
    synth_pg_class, synth_pg_constraint, synth_pg_database, synth_pg_extension, synth_pg_index_raw,
    synth_pg_indexes, synth_pg_namespace, synth_pg_operator, synth_pg_proc, synth_pg_roles,
    synth_pg_sequence, synth_pg_settings, synth_pg_timezone_abbrevs, synth_pg_timezone_names,
    synth_pg_trigger, synth_pg_type, synth_pg_views, topk_trim, try_gin_jsonb_seek, try_gin_seek,
    try_index_seek, try_nsw_knn, try_pk_walk_top_n, try_trgm_seek, value_is_bigint,
    value_is_integer, value_to_i64,
};

/// v7.39 (round 618) — a recursive term that can be run over the working set
/// directly, instead of through a whole query execution per round.
///
/// PG plans the recursive term ONCE and re-scans a worktable each iteration.
/// SPG emptied and refilled a real table and then called `exec_select_cancel`
/// — FROM resolution, schema build, predicate compilation, projection build
/// and result materialisation — for every round. Measured with the counting
/// allocator on `WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r
/// WHERE n < N)`: about 40 allocations and 99 kB PER ROUND while the working
/// set is one row, or 1.98 GB at N = 20000.
///
/// This is the shape that covers the ordinary recursive term: read the CTE,
/// filter it, project it. Anything else — a join, an aggregate, a window, a
/// subquery, DISTINCT, GROUP BY, ORDER BY, LIMIT, a locking clause, a
/// non-table source — returns `None` and keeps the general path, so the
/// answers it gives are the ones that path gave.
struct RecursiveTermPlan<'t> {
    items: Vec<&'t Expr>,
    where_: Option<&'t Expr>,
    alias: String,
}

fn plan_recursive_term<'t>(
    t: &'t SelectStatement,
    cte_name: &str,
    ncols: usize,
) -> Option<RecursiveTermPlan<'t>> {
    if !t.unions.is_empty()
        || !t.ctes.is_empty()
        || t.distinct
        || !t.distinct_on.is_empty()
        || t.group_by.is_some()
        || t.group_by_all
        || t.having.is_some()
        || !t.order_by.is_empty()
        || t.limit.is_some()
        || t.offset.is_some()
        || t.limit_with_ties
        || t.locking.is_some()
    {
        return None;
    }
    let from = t.from.as_ref()?;
    if !from.joins.is_empty() {
        return None;
    }
    let p = &from.primary;
    if !p.name.eq_ignore_ascii_case(cte_name)
        || p.as_of_segment.is_some()
        || p.unnest_expr.is_some()
        || !p.unnest_column_aliases.is_empty()
        || p.with_ordinality
        || p.generate_series_args.is_some()
        || p.lateral_subquery.is_some()
        || p.jsonb_each_text_arg.is_some()
        || p.table_fn_call.is_some()
    {
        return None;
    }
    let unsupported = |e: &Expr| {
        crate::aggregate::contains_aggregate(e)
            || crate::subquery::expr_has_subquery(e)
            || crate::window::expr_has_window_pub(e)
    };
    let mut items: Vec<&Expr> = Vec::with_capacity(t.items.len());
    for it in &t.items {
        match it {
            SelectItem::Expr { expr, .. } => {
                if unsupported(expr) {
                    return None;
                }
                items.push(expr);
            }
            // `*` would have to be expanded against the CTE's own schema;
            // the general path already does that, so leave it there.
            _ => return None,
        }
    }
    if items.len() != ncols {
        return None;
    }
    if let Some(w) = &t.where_
        && unsupported(w)
    {
        return None;
    }
    Some(RecursiveTermPlan {
        items,
        where_: t.where_.as_ref(),
        alias: p.alias.clone().unwrap_or_else(|| p.name.clone()),
    })
}

impl Engine {
    /// v4.12 window executor. Implements `ROW_NUMBER` / `RANK` /
    /// `DENSE_RANK` and the partition-aware aggregates `SUM` /
    /// `AVG` / `COUNT` / `MIN` / `MAX`. The plan is:
    /// 1. Apply the WHERE filter.
    /// 2. For each unique `WindowFunction` node in the projection,
    ///    partition + sort, compute the per-row value.
    /// 3. Append the window values as synthetic columns (`__win_N`)
    ///    to the row schema.
    /// 4. Rewrite the projection to read those columns.
    /// 5. Hand off to the regular project / ORDER BY / LIMIT pipe.
    #[allow(
        clippy::too_many_lines,
        clippy::type_complexity,
        clippy::needless_range_loop
    )] // window-eval is one cohesive pipe; splitting fragments
    pub(crate) fn exec_select_with_window(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let from = stmt.from.as_ref().ok_or_else(|| {
            EngineError::Unsupported("window functions require a FROM clause".into())
        })?;
        // v7.17.0 Phase 3.P0-43 — JOIN + window functions. Phase
        // 3.6 rejected this combination outright ("queued for
        // v5.x"); P0-43 materialises the join + WHERE through the
        // existing nested-loop helper and runs the window pipeline
        // on the joined row set with the combined `alias.col`
        // schema. The window expressions resolve through the
        // qualifier-aware column resolver same as the aggregate /
        // projection paths on JOIN.
        let (schema_cols_owned, alias_opt): (Vec<ColumnSchema>, Option<&str>);
        // v7.39 (round 976) — rows this walk OWNS. A derived FROM item and
        // a JOIN both produce rows that exist nowhere else, so they land
        // here; a plain stored table does not, and borrows instead.
        //
        // It used to clone every row out of the table, on the reasoning
        // that "the clone is cheap relative to the window computation that
        // follows". Measured on 400k rows, `row_number() OVER ()` cost
        // 31.881 ms against 46.520 with a 200-byte column added — so the
        // clone tracks row width at about 36 ns per row per 200 bytes, and
        // the window computation it was being compared against is a
        // counter increment per row. Nothing downstream needs the rows
        // owned: the very next statement used to be
        // `filtered.iter().collect()` into the `&Row` slice the window
        // pipeline actually reads.
        let mut owned_rows: Vec<Row<'static>> = Vec::new();
        // What the pipeline reads. Borrows `owned_rows` or the table.
        let mut filtered: Vec<&Row<'static>> = Vec::new();
        // Set by the branches that fill `owned_rows`, because "empty" is
        // an answer a query can legitimately have and so cannot be the
        // signal for which of the two holds the rows.
        let mut rows_are_owned = false;
        if from.joins.is_empty() {
            let primary = &from.primary;
            // v7.37 D.13 — window functions over a derived table (subquery /
            // VALUES / unnest / generate_series). The catalog-by-name lookup
            // below only finds real tables, so a derived primary threw
            // TableNotFound. Materialise the derived rows + schema through the
            // same helper the non-window FROM-primary path uses, then WHERE-
            // filter and feed the identical window pipeline.
            let is_derived = primary.lateral_subquery.is_some()
                || primary.unnest_expr.is_some()
                || primary.generate_series_args.is_some()
                || primary.jsonb_each_text_arg.is_some()
                || primary.table_fn_call.is_some();
            if is_derived {
                let (drows, dcols) = self.materialise_table_ref(primary)?;
                schema_cols_owned = dcols;
                alias_opt = primary.alias.as_deref();
                let ctx = self.ev_ctx(&schema_cols_owned, alias_opt);
                let mut owned: Vec<Row<'static>> = Vec::new();
                for (i, row) in drows.into_iter().enumerate() {
                    if i.is_multiple_of(256) {
                        cancel.check()?;
                    }
                    if let Some(w) = &stmt.where_ {
                        let cond = eval::eval_expr(w, &row, &ctx)?;
                        if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                            continue;
                        }
                    }
                    owned.push(row);
                }
                owned_rows = owned;
                rows_are_owned = true;
            } else {
                let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
                    StorageError::TableNotFound {
                        name: primary.name.clone(),
                    }
                })?;
                let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
                schema_cols_owned = table.schema().columns.clone();
                alias_opt = Some(alias);
                let ctx = self.ev_ctx(&schema_cols_owned, alias_opt);
                // The WHERE test, in ONE place, for all four ways a row can
                // reach this walk. It deliberately does not touch the row
                // collections: a closure that pushed into them would tie
                // its argument to the closure body and no borrowed row
                // could escape it, which is what forced the clone-shaped
                // version of this loop in the first place.
                let passes = |row: &Row<'static>| -> Result<bool, EngineError> {
                    if let Some(w) = &stmt.where_ {
                        let cond = eval::eval_expr(w, row, &ctx)?;
                        if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                };
                // v7.37.15 Phase B — scan_visible filters rows by the
                // engine's current snapshot. Phase B's `current_snapshot()`
                // returns `Snapshot::unbounded()` so every row is visible,
                // matching pre-v7.37.15 byte-for-byte. Phase C will wire
                // real per-tx snapshots through this same callsite — no
                // code change needed here when that lands.
                let snap = self.current_snapshot();
                if table.has_cold_rows_fast() {
                    // v7.36 (cold-tier coverage) — a cold segment's rows
                    // are produced on demand and live in a temporary this
                    // walk cannot borrow from, so a table carrying any owns
                    // its rows. Hot iter then cold iter, both through the
                    // same WHERE, as before.
                    let mut owned: Vec<Row<'static>> = Vec::new();
                    for (i, row) in table.scan_visible(&snap) {
                        if i.is_multiple_of(256) {
                            cancel.check()?;
                        }
                        if passes(row)? {
                            owned.push(row.clone());
                        }
                    }
                    let hot_len = table.row_count();
                    for (offset, row) in self.iter_cold_rows_of_table(table).iter().enumerate() {
                        let i = hot_len + offset;
                        if i.is_multiple_of(256) {
                            cancel.check()?;
                        }
                        if passes(row)? {
                            owned.push(row.clone());
                        }
                    }
                    owned_rows = owned;
                    rows_are_owned = true;
                } else {
                    // v7.39 (round 975) — ask the indices first, the way
                    // the streaming walk has since round 970. This walk had
                    // the same hole and it is reached by any statement
                    // carrying a window function, so a WHERE that names an
                    // indexed column read the whole table: measured on 400k
                    // rows, `row_number() OVER () … WHERE id = 500` — a
                    // ONE-row answer on a primary key — took 13.762 ms
                    // against PG18.4's 0.151, while the same predicate
                    // without the window took 0.091. The cost was
                    // independent of how many rows survived (999 survivors
                    // cost 13.312 ms) and of row width (13.312 narrow vs
                    // 13.327 wide), which is what a full table walk looks
                    // like and what a result-shaped cost does not.
                    //
                    // The seek only NARROWS — `passes` still applies the
                    // whole WHERE — so no answer can change. Positions
                    // arrive visibility-filtered by the same predicate the
                    // scan applies and capped at a quarter of the table,
                    // and `None` walks the table exactly as before.
                    let seek_positions: Option<Vec<usize>> = stmt.where_.as_ref().and_then(|w| {
                        crate::index_access::try_index_seek_positions(
                            w,
                            &schema_cols_owned,
                            table,
                            alias,
                            &snap,
                        )
                    });
                    match seek_positions {
                        Some(mut positions) => {
                            // Table order, which is the order the scan
                            // would have produced.
                            positions.sort_unstable();
                            for (n, pos) in positions.into_iter().enumerate() {
                                if n.is_multiple_of(256) {
                                    cancel.check()?;
                                }
                                let Some(row) = table.rows().get(pos) else {
                                    continue;
                                };
                                if passes(row)? {
                                    filtered.push(row);
                                }
                            }
                        }
                        None => {
                            for (i, row) in table.scan_visible(&snap) {
                                if i.is_multiple_of(256) {
                                    cancel.check()?;
                                }
                                if passes(row)? {
                                    filtered.push(row);
                                }
                            }
                        }
                    }
                }
            }
        } else {
            let deferred = self.build_joined_filtered_rows(
                from,
                stmt.where_.as_ref(),
                cancel,
                None,
                &mut ByteBudget::new(self.max_query_bytes),
            )?;
            // A join's survivors are row-index tuples over its sources, so
            // there is no single row to borrow — this branch owns them.
            owned_rows = deferred.materialise();
            rows_are_owned = true;
            schema_cols_owned = deferred.combined_schema;
            alias_opt = None;
        }
        if rows_are_owned {
            filtered = owned_rows.iter().collect();
        }
        let schema_cols = &schema_cols_owned;
        let ctx = self.ev_ctx(schema_cols, alias_opt);
        let alias = alias_opt.unwrap_or("");
        let n_rows = filtered.len();
        // The window pipeline reads `&[&Row<'static>]`, and `filtered`
        // already is one whichever branch produced it — the separate
        // `filtered_refs` this used to build was the collect that made
        // owning the rows look necessary.

        // 2) Collect unique window function nodes from projection.
        let mut window_nodes: Vec<Expr> = Vec::new();
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_window_nodes(expr, &mut window_nodes);
            }
        }
        // v7.39 (round 592) — and from ORDER BY, which may name a window the
        // select list never mentions. The order-key builder below rewrites
        // window calls to `__win_N` columns, and a call that was never
        // collected has no column to become.
        for o in &stmt.order_by {
            collect_window_nodes(&o.expr, &mut window_nodes);
        }

        // 3) For each window, compute per-row value.
        // Index: same order as window_nodes; for row i, win_vals[w][i].
        let mut win_vals: Vec<Vec<Value<'static>>> = Vec::with_capacity(window_nodes.len());
        for wnode in &window_nodes {
            let Expr::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
                null_treatment,
                filter,
            } = wnode
            else {
                unreachable!("collect_window_nodes pushes only WindowFunction");
            };
            // Compute (partition_key, order_key, original_index) for each row.
            // v7.39 (round 593) — a key that is a plain column sits at the same
            // position in every row, but was resolved BY NAME for each one. A
            // per-library profile of `lag(id) OVER (ORDER BY id)` put
            // `resolve_column` at 5.8% of the query on its own, with
            // `rehydrate_cell` and the `eval_expr` dispatch behind it. Resolve
            // once; anything that is not a plain column keeps the resolver.
            let p_bound: Vec<Option<usize>> = partition_by
                .iter()
                .map(|e| crate::orderby::bound_column_position(e, schema_cols, alias_opt))
                .collect();
            let o_bound: Vec<Option<usize>> = order_by
                .iter()
                .map(|(e, _, _)| crate::orderby::bound_column_position(e, schema_cols, alias_opt))
                .collect();
            let arg_bound = args
                .first()
                .and_then(|a| crate::orderby::bound_column_position(a, schema_cols, alias_opt));
            // v7.39 (round 690) — a window's ORDER BY over a column that
            // declares a collation sorts by it, the same as a top-level
            // ORDER BY. Resolved from the bound position, so only a bare
            // column gets one; an expression produces a new value and the
            // derivation that would give IT a collation is unbuilt.
            let o_colls: Vec<Option<alloc::string::String>> = o_bound
                .iter()
                .map(|p| {
                    p.and_then(|pos| schema_cols.get(pos))
                        .and_then(|sc| sc.collation_name.clone())
                        .filter(|n| crate::collate::is_supported(n))
                })
                .collect();
            let mut indexed: Vec<(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)> =
                Vec::with_capacity(n_rows);
            // v7.39 (round 731) — single bound INT partition key, no window
            // ORDER BY: group on the i64 directly. The generic build paid
            // two heap Vecs per row (pkey + empty okey) plus a canonical
            // string encode per row just to bucket 500k rows into 100
            // groups; the whole per-row key apparatus disappears here.
            // Neither key Vec is read downstream on this path: the hash
            // grouping replaces partition_key_cmp, and okey is empty by
            // construction.
            let int_pkey_fast = order_by.is_empty()
                && partition_by.len() == 1
                && p_bound[0].is_some_and(|pos| {
                    matches!(
                        schema_cols.get(pos).map(|c| c.ty),
                        Some(
                            spg_storage::DataType::Int
                                | spg_storage::DataType::BigInt
                                | spg_storage::DataType::SmallInt
                        )
                    )
                });
            // v7.39 (round 979) — the same idea for a single bound INT
            // window ORDER BY: sort on the i64 instead of on a heap vector
            // per row.
            //
            // Measured at 400k rows (round 978, ablation, answer checked
            // byte-for-byte against the general path on a key column that
            // is a permutation): `row_number() OVER (ORDER BY k)` went
            // 157.057-157.868 ms to 31.253-31.679, which is 79.8% and puts
            // it on top of the `OVER ()` baseline — the sort essentially
            // disappears. Round 977 had already shown the cost was
            // key-shaped rather than row-shaped: the sort's share was
            // 132.0 ms on a three-integer table and 132.5 with a 200-byte
            // column added, and a per-row COPY does scale with width
            // (round 976 measured that at +36 ns/row/200 bytes).
            //
            // Gated to ROW_NUMBER, which is the one function that reads
            // neither key vector — it numbers the order it is handed.
            // `rank` and `dense_rank` compare adjacent entries' order keys
            // in `compute_window_partition`, so leaving those vectors
            // empty would silently give every row rank 1. A wider version
            // would carry the i64 in the entry and teach those two to use
            // it; this one is the part that can be shown correct by
            // construction.
            let int_okey_fast = partition_by.is_empty()
                && order_by.len() == 1
                && frame.is_none()
                && filter.is_none()
                && matches!(null_treatment, spg_sql::ast::NullTreatment::Respect)
                && name.eq_ignore_ascii_case("row_number")
                && o_bound[0].is_some_and(|pos| {
                    matches!(
                        schema_cols.get(pos).map(|c| c.ty),
                        Some(
                            spg_storage::DataType::Int
                                | spg_storage::DataType::BigInt
                                | spg_storage::DataType::SmallInt
                        )
                    )
                });
            // Set when a cell in that column turns out not to be an
            // integer after all. The declared type says it should be, but
            // "should" is not a thing to sort 400k rows on, so the general
            // path takes over and this build is discarded.
            let mut int_okey_bailed = false;
            if int_okey_fast {
                let pos = o_bound[0].expect("gated bound");
                let desc = order_by[0].1;
                // PG orders NULLs last ascending and first descending
                // unless the query says otherwise.
                let nulls_first = order_by[0].2.unwrap_or(desc);
                let mut keyed: Vec<(bool, i64, usize)> = Vec::with_capacity(n_rows);
                for (i, row) in filtered.iter().enumerate() {
                    match row.values.get(pos) {
                        Some(Value::Int(n)) => keyed.push((false, i64::from(*n), i)),
                        Some(Value::BigInt(n)) => keyed.push((false, *n, i)),
                        Some(Value::SmallInt(n)) => keyed.push((false, i64::from(*n), i)),
                        Some(Value::Null) | None => keyed.push((true, 0, i)),
                        Some(_) => {
                            int_okey_bailed = true;
                            break;
                        }
                    }
                }
                if !int_okey_bailed {
                    // `null_rank` puts NULLs on the side the query asked
                    // for; the row's original index breaks every tie, so
                    // equal keys keep the order the scan produced — what
                    // the stable sort below would have given them.
                    let null_rank = |is_null: bool| -> u8 { u8::from(is_null != nulls_first) };
                    keyed.sort_unstable_by(|a, b| {
                        null_rank(a.0)
                            .cmp(&null_rank(b.0))
                            .then_with(|| {
                                if a.0 {
                                    core::cmp::Ordering::Equal
                                } else if desc {
                                    b.1.cmp(&a.1)
                                } else {
                                    a.1.cmp(&b.1)
                                }
                            })
                            .then_with(|| a.2.cmp(&b.2))
                    });
                    for (_, _, i) in keyed {
                        indexed.push((Vec::new(), Vec::new(), i));
                    }
                } else {
                    indexed.clear();
                }
            }
            if int_okey_fast && !int_okey_bailed {
                // Ordered above; nothing else to build.
            } else if int_pkey_fast {
                let pos = p_bound[0].expect("gated bound");
                let mut slot: hashbrown::HashMap<Option<i64>, usize> = hashbrown::HashMap::new();
                let mut groups: Vec<Vec<usize>> = Vec::new();
                for (i, row) in filtered.iter().enumerate() {
                    let k: Option<i64> = match row.values.get(pos) {
                        Some(Value::BigInt(n)) => Some(*n),
                        Some(Value::Int(n)) => Some(i64::from(*n)),
                        Some(Value::SmallInt(n)) => Some(i64::from(*n)),
                        _ => None,
                    };
                    match slot.get(&k) {
                        Some(&gi) => groups[gi].push(i),
                        None => {
                            slot.insert(k, groups.len());
                            groups.push(alloc::vec![i]);
                        }
                    }
                }
                // The downstream partition-boundary scan compares pkeys
                // of ADJACENT entries, so the key must ride along — one
                // single-element Vec per row (half the generic build's
                // allocations, no string encode).
                for g in groups {
                    for i in g {
                        let k: Value<'static> = match filtered[i].values.get(pos) {
                            Some(v) => v.clone(),
                            None => Value::Null,
                        };
                        indexed.push((alloc::vec![k], Vec::new(), i));
                    }
                }
            } else {
                for (i, row) in filtered.iter().enumerate() {
                    let pkey: Vec<Value<'static>> = partition_by
                        .iter()
                        .enumerate()
                        .map(
                            |(k, p)| match p_bound[k].and_then(|pos| row.values.get(pos)) {
                                Some(v) => Ok(v.clone()),
                                None => eval::eval_expr(p, row, &ctx),
                            },
                        )
                        .collect::<Result<_, _>>()?;
                    // v7.39 (read01 round 54) — a window's ORDER BY over an enum
                    // column must sort by MEMBER order (enumsortorder), not the
                    // label's text. Enum values are Text at runtime, so the raw
                    // value key sorted alphabetically — `row_number() OVER (ORDER
                    // BY mood)` numbered the rows happy,ok,sad. Substitute the
                    // member ordinal, the same key the top-level ORDER BY uses.
                    // (Closes the enum-order knife's recorded window residual.)
                    let okey: Vec<(Value, bool, Option<bool>)> = order_by
                        .iter()
                        .enumerate()
                        .map(|(k, (e, desc, nf))| -> Result<_, EngineError> {
                            let v = match o_bound[k].and_then(|pos| row.values.get(pos)) {
                                Some(v) => v.clone(),
                                None => eval::eval_expr(e, row, &ctx)?,
                            };
                            let v = match crate::orderby::enum_order_ordinal(e, &v, &ctx) {
                                Some(ord) => Value::Float(ord),
                                None => v,
                            };
                            Ok((v, *desc, *nf))
                        })
                        .collect::<Result<_, _>>()?;
                    indexed.push((pkey, okey, i));
                }
            }
            // Sort by (partition_key, order_key). Partition key uses
            // a stable encoded form; order key respects ASC/DESC.
            // v7.39 (round 731) — with NO window ORDER BY the sort's only
            // job was putting same-partition rows next to each other, and a
            // 500k-row comparison sort is a spectacular way to hash-group:
            // the panel's `sum(id) OVER (PARTITION BY g)` spent ~100 ms
            // here. Group by encoded key instead, preserving row order
            // inside each group — exactly what the stable sort preserved,
            // so every function (row_number included) answers the same.
            if int_okey_fast && !int_okey_bailed {
                // Already ordered by the i64 key above.
            } else if int_pkey_fast {
                // Already grouped above; same-partition rows are adjacent
                // in original row order.
            } else if order_by.is_empty() && !partition_by.is_empty() {
                let mut slot: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
                let mut groups: Vec<
                    Vec<(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)>,
                > = Vec::new();
                let mut keybuf = String::new();
                for entry in indexed.drain(..) {
                    keybuf.clear();
                    for v in &entry.0 {
                        crate::aggregate::push_canonical_key(&mut keybuf, v);
                    }
                    match slot.get(keybuf.as_str()) {
                        Some(&gi) => groups[gi].push(entry),
                        None => {
                            slot.insert(keybuf.clone(), groups.len());
                            groups.push(alloc::vec![entry]);
                        }
                    }
                }
                for g in groups {
                    indexed.extend(g);
                }
            } else {
                indexed.sort_by(|a, b| {
                    let p_cmp = partition_key_cmp(&a.0, &b.0);
                    if p_cmp != core::cmp::Ordering::Equal {
                        return p_cmp;
                    }
                    crate::window::order_key_cmp_in(&a.1, &b.1, &o_colls)
                });
            }
            // Per-partition compute.
            let mut out_vals: Vec<Value<'static>> = alloc::vec![Value::Null; n_rows];
            let mut p_start = 0;
            while p_start < indexed.len() {
                let mut p_end = p_start + 1;
                while p_end < indexed.len()
                    && partition_key_cmp(&indexed[p_start].0, &indexed[p_end].0)
                        == core::cmp::Ordering::Equal
                {
                    p_end += 1;
                }
                // Compute the function within this partition slice.
                compute_window_partition(
                    name,
                    args,
                    arg_bound,
                    !order_by.is_empty(),
                    frame.as_ref(),
                    *null_treatment,
                    filter.as_deref(),
                    &indexed[p_start..p_end],
                    &filtered,
                    &ctx,
                    &mut out_vals,
                )?;
                p_start = p_end;
            }
            win_vals.push(out_vals);
        }

        // 4) Build extended schema: original columns + synthetic.
        let mut ext_cols = schema_cols.clone();
        for i in 0..window_nodes.len() {
            ext_cols.push(ColumnSchema::new(
                alloc::format!("__win_{i}"),
                DataType::Text, // type doesn't matter for projection eval
                true,
            ));
        }
        // 6) Rewrite the projection: WindowFunction nodes → Column(__win_N).
        let mut rewritten_items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
        for item in &stmt.items {
            let new_item = match item {
                SelectItem::Wildcard => SelectItem::Wildcard,
                SelectItem::QualifiedWildcard(q) => SelectItem::QualifiedWildcard(q.clone()),
                SelectItem::Expr { expr, alias } => {
                    let mut e = expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    // The rewrite swaps the window call for a synthetic
                    // `__win_N` column, and the projection then reported
                    // THAT as the column name — `SELECT count(*) OVER ()`
                    // answered `__win_0`, an internal name, where PG18
                    // answers `count`. Pin the name while the call the
                    // column is named for is still in hand.
                    let alias = if alias.is_none() && e != *expr {
                        Some(default_output_name(expr, self.backslash_escapes))
                    } else {
                        alias.clone()
                    };
                    SelectItem::Expr { expr: e, alias }
                }
            };
            rewritten_items.push(new_item);
        }

        // 7) Project into final rows. JOIN case uses None so the
        // qualifier check in `resolve_column` falls through to the
        // composite `alias.col` schema lookup; single-table case
        // keeps the bare alias so `bare_col` resolution still
        // works for the projection's per-row column references.
        // v7.39 (read01 round 54) — build through `ev_ctx`, the canonical
        // constructor: it threads the catalog (plus render style / tz / GUCs)
        // that a bare `EvalContext::new` drops. Without the catalog the OUTER
        // `ORDER BY <enum col>` of a windowed query sorted by TEXT — the
        // window values were right, the row order silently was not.
        let ext_ctx = self.ev_ctx(&ext_cols, alias_opt);
        let projection = build_projection_hiding_tail(
            &rewritten_items,
            &ext_cols,
            alias,
            self.backslash_escapes,
            window_nodes.len(),
        )?;
        let mut tagged: Vec<(Vec<OrderKey>, Row)> = Vec::with_capacity(n_rows);
        // v7.39 (round 592) — the extended row (input columns plus the window
        // values) used to be materialised for EVERY input row and kept until
        // the projection had run: the input values cloned into a fresh Vec,
        // then grown once to take the window columns. A counting allocator put
        // the window path at 4 allocations a row where a plain derived table
        // takes 1, and named all four — the input row, the clone, the growth,
        // and the projected row. Only the last has to exist afterwards, so the
        // extended row is one buffer refilled per row.
        let mut ext_row: Row<'static> =
            Row::new(Vec::with_capacity(schema_cols.len() + window_nodes.len()));
        for i in 0..n_rows {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            ext_row.values.clear();
            ext_row.values.extend(filtered[i].values.iter().cloned());
            for w in 0..window_nodes.len() {
                ext_row.values.push(win_vals[w][i].clone());
            }
            let row = &ext_row;
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ext_ctx)?);
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                let mut keys = Vec::with_capacity(stmt.order_by.len());
                for o in &stmt.order_by {
                    let mut e = o.expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    let key = eval::eval_expr(&e, row, &ext_ctx)?;
                    // v7.39 (read01 round 54) — this path builds its order keys
                    // itself instead of going through `build_order_keys`, so it
                    // skipped the enum-ordinal substitution: the OUTER
                    // `ORDER BY <enum col>` of a windowed query sorted by the
                    // label's TEXT, not by member order. The window values were
                    // right and only the row order was wrong — silently.
                    match crate::orderby::enum_order_ordinal(&e, &key, &ext_ctx) {
                        Some(ord) => keys.push(value_to_order_key(&Value::Float(ord))?),
                        None => keys.push(value_to_order_key(&key)?),
                    }
                }
                keys
            };
            tagged.push((order_keys, Row::new(values)));
        }
        // ORDER BY + LIMIT/OFFSET on the projected rows.
        if !stmt.order_by.is_empty() {
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            sort_by_keys(&mut tagged, &descs);
        }
        let mut out_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
        // v7.37 D.41 — `SELECT DISTINCT` over a window projection: the window
        // pipeline builds one output row per input row, so DISTINCT must dedup the
        // projected rows (PG evaluates window functions before DISTINCT). Applied
        // after ORDER BY (duplicate rows share sort keys, so order is preserved)
        // and before LIMIT.
        if stmt.distinct {
            out_rows = dedup_rows(out_rows, self.backslash_escapes);
        }
        apply_offset_and_limit(&mut out_rows, stmt.offset_literal(), stmt.limit_literal());
        let final_cols: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type;
                c.collation_name = p.collation_name;
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        Ok(QueryResult::Rows {
            columns: final_cols,
            rows: out_rows,
        })
    }

    /// v4.11: materialise each CTE into a temp table inside a
    /// cloned catalog, then run the body SELECT against a fresh
    /// engine instance that owns the enriched catalog. The clone
    /// is moderately expensive — only paid by CTE-bearing queries.
    /// Subqueries inside CTE bodies / the main body resolve as
    /// usual; `clock_fn` is propagated so `NOW()` lines up.
    /// v7.16.2 — mailrs round-10 A.3. Materialise the
    /// `information_schema.*` / `pg_catalog.*` virtual views
    /// the SELECT references, then re-execute the SELECT
    /// against an enriched catalog where those views are real
    /// tables. Same pattern as `exec_with_ctes`. The temp
    /// engine carries `meta_views_materialised = true` so its
    /// own meta-dispatch short-circuits — without that we'd
    /// infinite-recurse since the temp catalog's view name
    /// still starts with `__spg_info_` and re-triggers the
    /// check.
    pub(crate) fn exec_select_with_meta_views(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let catalog = self.meta_view_catalog(stmt)?;
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        // v7.39 (round 522) — the temp engine holds the materialised
        // catalog and, until now, nothing of the SESSION. So every
        // session-scoped answer changed the moment a system view
        // appeared in the FROM clause: `SELECT current_user` said
        // `unmei` and `SELECT current_user FROM pg_class` said `admin`;
        // `current_setting('work_mem')` fell back to the boot default
        // after a SET; `application_name` read empty. A privilege check
        // written against a catalog join was reading a different
        // identity than the same check written without one.
        //
        // Carry what a session can be observed through — its parameters
        // (which is also where the session user lives), the role store
        // the privilege builtins read, the dialect, and the rendering
        // settings a timestamp is spelled with.
        temp.session_params.clone_from(&self.session_params);
        temp.users.clone_from(&self.users);
        temp.backslash_escapes = self.backslash_escapes;
        temp.mysql_strict = self.mysql_strict;
        temp.render_style = self.render_style;
        temp.tz_offset_fn = self.tz_offset_fn;
        temp.tz_localize_fn = self.tz_localize_fn;
        temp.tz_abbrev_fn = self.tz_abbrev_fn;
        temp.meta_views_materialised = true;
        temp.exec_select_cancel(stmt, cancel)
    }

    /// v7.39 (round 462) — the catalog a meta-view SELECT resolves
    /// against: this engine's catalog with every `__spg_*` view the
    /// statement references materialised into it.
    ///
    /// Split out of `exec_select_with_meta_views` so Describe can reach
    /// the same shapes execution reaches. Describe used to look the FROM
    /// relation up in the plain catalog, where a system view does not
    /// exist, and reported "no columns" for every one of them — so an
    /// extended-protocol client reading `pg_stat_user_tables` got rows
    /// with no column metadata. Sharing the materialisation means a
    /// view added here is described correctly the day it is added.
    pub(crate) fn meta_view_catalog(&self, stmt: &SelectStatement) -> Result<Catalog, EngineError> {
        let mut needed: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        collect_meta_view_names(stmt, &mut needed);
        let mut catalog = self.active_catalog().clone();
        for view in &needed {
            if catalog.get(view).is_some() {
                continue;
            }
            match view.as_str() {
                "__spg_info_columns" => {
                    let (schema, rows) = synth_information_schema_columns(
                        self.active_catalog(),
                        self.backslash_escapes,
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_info_tables" => {
                    let (schema, rows) = synth_information_schema_tables(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_class" => {
                    let (schema, rows) = synth_pg_class(
                        self.active_catalog(),
                        i64::try_from(self.vacuum_oldest_active()).unwrap_or(i64::MAX),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_attribute" => {
                    let (schema, rows) = synth_pg_attribute(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-50 — pg_catalog.pg_type for
                // sqlx / SQLAlchemy / Diesel / pgAdmin lookups.
                "__spg_pg_type" => {
                    let (schema, rows) = synth_pg_type(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 621) — pg_catalog.pg_operator, which did not
                // exist at all.
                "__spg_pg_operator" => {
                    let (schema, rows) = synth_pg_operator(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-51 — pg_catalog.pg_proc for
                // function-name introspection (ORM / pgAdmin).
                "__spg_pg_proc" => {
                    let (schema, rows) = synth_pg_proc(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.24 (round-16 D) — pg_catalog.pg_trigger. The
                // round-16 "why doesn't prod fire the trigger"
                // question was unanswerable because triggers had NO
                // introspection surface; tgname/tgenabled plus the
                // pragmatic relname/timing/events/function columns
                // make "is it registered and enabled" a one-liner.
                "__spg_pg_trigger" => {
                    let (schema, rows) = synth_pg_trigger(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-52 — pg_catalog.pg_namespace
                // (schema list for admin tools' tree views).
                "__spg_pg_namespace" => {
                    let (schema, rows) = synth_pg_namespace(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 — pg_tables convenience view (was a pgwire
                // canned response that ignored projections).
                "__spg_pg_tables" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_tables(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.1) — pg_catalog.pg_enum (label list
                // for ENUM types; sqlx / ORM enum codecs read this).
                "__spg_pg_enum" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_enum(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13) — pg_catalog.pg_replication_slots
                // (shape-stable empty until 21.12 persists slot state).
                // v7.39 (round 277) — session-scoped prepared statements.
                "__spg_pg_prepared_statements" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_prepared_statements(
                        &self.prepared_statements,
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_replication_slots" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_replication_slots(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13-b) — pg_catalog.pg_publication
                // (one row per CREATE PUBLICATION).
                "__spg_pg_publication" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_publication(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13-c) — pg_catalog.pg_subscription
                // (one row per CREATE SUBSCRIPTION; subconninfo
                // redacted so dashboards can't leak credentials).
                "__spg_pg_subscription" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_subscription(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.x-stat-db) — pg_catalog.pg_stat_database
                // (one row for SPG's single database; counters are
                // shape-stable 0 until wiring lands).
                "__spg_pg_stat_database" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_stat_database(
                        self,
                        self.stat_tup_inserted,
                        self.stat_tup_updated,
                        self.stat_tup_deleted,
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.14) — pg_catalog.pg_stat_user_tables
                // (per-table churn counters; live_tup = row count).
                "__spg_pg_stat_user_tables" => {
                    // r192 — DML counters come from the engine-side
                    // non-transactional map, not the (tx-shadowed)
                    // catalog tables.
                    let (schema, rows) = crate::system_catalog::synth_pg_stat_user_tables(
                        self.active_catalog(),
                        &self.table_write_stats,
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.15) — pg_catalog.pg_stat_user_indexes
                // (per-index usage counters; flag unused indexes).
                "__spg_pg_stat_user_indexes" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_user_indexes(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.16) — pg_catalog.pg_stat_bgwriter.
                "__spg_pg_stat_bgwriter" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_bgwriter(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.38 (read01 P3.14) — pg_catalog.pg_stat_checkpointer /
                // pg_stat_wal shell views (shape-stable, counters pending).
                "__spg_pg_stat_checkpointer" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_checkpointer(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_stat_wal" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_wal(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.38 (read01 P3.15) — pg_catalog.pg_stat_slru /
                // pg_stat_subscription_stats shell views.
                "__spg_pg_stat_slru" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_slru(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_stat_subscription_stats" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_stat_subscription_stats(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.17) — pg_catalog.pg_stat_archiver.
                "__spg_pg_stat_archiver" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_archiver(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13-d) — pg_catalog.pg_stat_replication.
                "__spg_pg_stat_replication" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_replication(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.13) — pg_catalog.pg_am.
                "__spg_pg_am" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_am(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.18) — pg_catalog.pg_stat_io (PG 16+).
                "__spg_pg_stat_io" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_io(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.19) — pg_catalog.pg_stat_user_functions.
                "__spg_pg_stat_user_functions" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_user_functions(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 287) — pg_catalog.pg_largeobject{,_metadata}.
                "__spg_pg_largeobject" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_largeobject(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_largeobject_metadata" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_largeobject_metadata(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.23 (23.7-a) — pg_catalog.pg_statistic_ext.
                "__spg_pg_statistic_ext" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_statistic_ext(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.15) — pg_catalog.pg_statistic.
                "__spg_pg_statistic" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_statistic(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.20) — pg_catalog.pg_stat_progress_vacuum.
                "__spg_pg_stat_progress_vacuum" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_progress_vacuum(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.21) — pg_catalog.pg_stat_progress_create_index.
                "__spg_pg_stat_progress_create_index" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_stat_progress_create_index(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.22) — pg_catalog.pg_stat_progress_analyze.
                "__spg_pg_stat_progress_analyze" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_stat_progress_analyze(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.16) — pg_catalog.pg_inherits
                // (partition parent → child OID mapping).
                "__spg_pg_inherits" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_inherits(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 650) — the text-search catalogs, filled
                // with what SPG actually has rather than PG's thirty.
                "__spg_pg_ts_config_map" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_ts_config_map(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_ts_config" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_ts_config(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_ts_dict" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_ts_dict(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_ts_parser" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_ts_parser(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_ts_template" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_ts_template(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.17) — pg_catalog.pg_depend
                // (dependency graph; shape-stable empty since
                // SPG's drop enforcement is per-kind, not per-object).
                "__spg_pg_depend" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_depend(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.38 (read01) — pg_catalog.pg_attrdef (column defaults;
                // ORM reflection + pg_dump read the deparsed default text).
                "__spg_pg_attrdef" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_attrdef(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (RLS) — pg_catalog.pg_policy (raw) + pg_policies (view).
                "__spg_pg_policy" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_policy(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_policies" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_policies(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.14) — pg_catalog.pg_collation.
                "__spg_pg_collation" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_collation(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.23 (23.6-b) — pg_catalog.pg_tablespace.
                "__spg_pg_tablespace" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_tablespace(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-53 — pg_catalog.pg_indexes view
                // for pgAdmin / DataGrip "indexes per table" listings.
                "__spg_pg_indexes" => {
                    let (schema, rows) = synth_pg_indexes(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (read01 round 50) — pg_catalog.pg_description, backing
                // psql's \d+ comment column and pg_dump's COMMENT ON emission.
                "__spg_pg_description" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_description(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-53 — pg_catalog.pg_index (raw)
                // for index introspection by ORM compilers.
                "__spg_pg_index" => {
                    let (schema, rows) = synth_pg_index_raw(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-54 — pg_catalog.pg_constraint
                // for FK / UNIQUE / PK / CHECK introspection.
                "__spg_pg_constraint" => {
                    let (schema, rows) = synth_pg_constraint(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37 U11 — pg_catalog.pg_sequence, one row per CREATE
                // SEQUENCE (psql \d <seq> + ORM sequence introspection).
                "__spg_pg_sequence" => {
                    let (schema, rows) = synth_pg_sequence(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-55 — pg_catalog.pg_database /
                // pg_roles / pg_user. SPG is single-database so
                // pg_database surfaces just `postgres`; pg_roles
                // / pg_user walk the engine's UserStore.
                "__spg_pg_database" => {
                    let (schema, rows) = synth_pg_database(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_roles" => {
                    let (schema, rows) = synth_pg_roles(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 542) — pg_user is a DIFFERENT view over the
                // same roles, with PG's own `use*` column names. It used to
                // publish pg_roles' columns under this name.
                "__spg_pg_user" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_user(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (read01 round 58) — role membership.
                "__spg_pg_auth_members" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_auth_members(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-56 — pg_catalog.pg_views. PG's
                // pg_views surfaces every CREATE VIEW result; SPG
                // ships one row per declared view from the catalog.
                "__spg_pg_views" => {
                    let (schema, rows) = synth_pg_views(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 143) — pg_catalog.pg_rules: one row per
                // catalogued query-rewrite RULE.
                "__spg_pg_rules" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_rules(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 312) — pg_catalog.pg_rewrite: the rule
                // catalogue `pg_get_ruledef(oid)` resolves against.
                "__spg_pg_rewrite" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_rewrite(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 542) — pg_catalog.pg_matviews, with rows
                // and PG's own column names.
                "__spg_pg_matviews" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_matviews(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // pg_catalog.pg_extension — native capability list
                // (mailrs embed round-12).
                // v7.39 (round 546) — the catalogs SPG has real content
                // for, from the facts it already holds.
                "__spg_pg_db_role_setting" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_db_role_setting(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_language" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_language();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_sequences" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_sequences(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_range" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_range();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_partitioned_table" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_partitioned_table(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_authid" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_authid(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_group" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_group(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_shadow" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_shadow(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 544) — pg_cast, probed from the real
                // cast implementation.
                "__spg_pg_cast" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_cast();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 541) — an empty catalog that exists.
                "__spg_pg_foreign_table" => {
                    let (schema, rows) = crate::system_catalog::synth_pg_foreign_table();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_extension" => {
                    let (schema, rows) = synth_pg_extension();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 502) — the timezone catalogues.
                "__spg_pg_timezone_names" => {
                    let (schema, rows) = synth_pg_timezone_names(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_timezone_abbrevs" => {
                    let (schema, rows) = synth_pg_timezone_abbrevs(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-57 — pg_catalog.pg_settings.
                "__spg_pg_settings" => {
                    let (schema, rows) = synth_pg_settings(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-63 — information_schema.KEY_COLUMN_USAGE.
                // v7.39 (read01 round 51) — information_schema.role_table_grants
                // and .table_privileges. Both report the owner's seven implicit
                // table privileges; SPG's single role owns everything.
                // v7.39 (read01 round 59) — information_schema.column_privileges.
                "__spg_info_column_privileges" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_info_column_privileges(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_info_role_table_grants" | "__spg_info_table_privileges" => {
                    let grantee = self.current_role().to_string();
                    let (schema, rows) = crate::system_catalog::synth_info_role_table_grants(
                        self.active_catalog(),
                        &grantee,
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_info_key_column_usage" => {
                    let (schema, rows) = synth_info_key_column_usage(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.REFERENTIAL_CONSTRAINTS.
                "__spg_info_referential_constraints" => {
                    let (schema, rows) = synth_info_referential_constraints(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.STATISTICS.
                "__spg_info_statistics" => {
                    let (schema, rows) = synth_info_statistics(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.ROUTINES.
                "__spg_info_routines" => {
                    let (schema, rows) = synth_info_routines();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.3) — information_schema.attributes.
                "__spg_info_attributes" => {
                    let (schema, rows) = crate::system_catalog::synth_information_schema_attributes(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.2) — information_schema.domains.
                "__spg_info_domains" => {
                    let (schema, rows) = crate::system_catalog::synth_information_schema_domains(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.9) — information_schema.schemata.
                "__spg_info_schemata" => {
                    let (schema, rows) = crate::system_catalog::synth_information_schema_schemata(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.9) — information_schema.views.
                "__spg_info_views" => {
                    let (schema, rows) = crate::system_catalog::synth_information_schema_views(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.9) — information_schema.table_constraints.
                "__spg_info_table_constraints" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_information_schema_table_constraints(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.17 — information_schema.constraint_column_usage.
                "__spg_info_constraint_column_usage" => {
                    let (schema, rows) = crate::system_catalog::synth_info_constraint_column_usage(
                        self.active_catalog(),
                    );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.17 — information_schema.triggers.
                "__spg_info_triggers" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_info_triggers(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.17 — information_schema.check_constraints.
                "__spg_info_check_constraints" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_info_check_constraints(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.17 — information_schema.sequences.
                "__spg_info_sequences" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_info_sequences(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-65 — mysql.user / mysql.db.
                "__spg_mysql_user" => {
                    let (schema, rows) = synth_mysql_user(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_mysql_db" => {
                    let (schema, rows) = synth_mysql_db();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.39 (round 541) — the catalogs PG has that SPG is
                // genuinely empty of. Table-driven; see EMPTY_PG_CATALOGS.
                other if crate::system_catalog::synth_empty_pg_catalog(other).is_some() => {
                    let (schema, rows) =
                        crate::system_catalog::synth_empty_pg_catalog(other).expect("just checked");
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "meta view {view:?} is not yet materialisable; \
                         v7.16.2 covers information_schema.columns / .tables \
                         and pg_catalog.pg_class / pg_attribute; \
                         v7.17.0 P0-50..P0-57 add pg_type / pg_proc / pg_namespace / \
                         pg_indexes / pg_index / pg_constraint / pg_database / pg_roles / \
                         pg_user / pg_views / pg_matviews / pg_settings"
                    )));
                }
            }
        }
        Ok(catalog)
    }

    pub(crate) fn exec_with_ctes(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v7.37.43-T4.4 — `&self` SELECT path: only read-only CTE
        // bodies are supported here. Writable CTEs on a SELECT
        // outer require `&mut self` and route through the
        // top-level `exec_select_cancel_mut` entry; sentori
        // 0065's WITH-INSERT-INSERT shape comes in as a top-level
        // INSERT, not a SELECT, so this restriction is harmless
        // in practice.
        if stmt.ctes.iter().any(|c| c.body.is_modifying()) {
            // v7.39 (read01 round 81) — PG's wording. A data-modifying CTE
            // (`WITH d AS (DELETE … RETURNING …) …`) is only legal at the top
            // of a statement, not nested inside a subquery; this path is
            // reached exactly when one is nested. The old text described SPG's
            // own executor plumbing ("the top-level mutable entry"), which
            // means nothing to a client.
            return Err(EngineError::Unsupported(
                "WITH clause containing a data-modifying statement must be at the top level".into(),
            ));
        }
        let catalog = self.materialise_ctes_readonly(&stmt.ctes, cancel)?;
        // Strip CTEs from the body before running on the temp engine
        // so we don't recurse forever.
        let mut body = stmt.clone();
        body.ctes = Vec::new();
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        temp.exec_select_cancel(&body, cancel)
    }

    /// v7.37.43-T4.4 — read-only CTE materialiser used by the
    /// `&self` SELECT path. Caller guarantees no modifying CTE
    /// bodies are present.
    pub(crate) fn materialise_ctes_readonly(
        &self,
        ctes: &[spg_sql::ast::Cte],
        cancel: CancelToken<'_>,
    ) -> Result<crate::Catalog, EngineError> {
        cancel.check()?;
        let mut catalog = self.active_catalog().clone();
        for cte in ctes {
            let body_select = cte.body.as_select().ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "data-modifying CTE not supported on this SELECT entry"
                ))
            })?;
            // v7.39 (round 156) — a CTE may SHADOW a same-named real table
            // (PG scoping: the WITH name wins for the outer query and later
            // CTEs, while THIS body still sees the real table — a
            // non-recursive body's self-name is the table, probe P2). This
            // materialiser works on a CLONE, so the shadow is simply: run
            // the body against the untouched clone, then drop the real
            // table from the clone before installing the CTE's temp. A
            // RECURSIVE self-reference is the CTE itself (P6), so there the
            // drop happens before the iterating materialiser runs.
            let (columns, rows) = if cte.recursive && select_refers_to(body_select, &cte.name) {
                let synthetic = spg_sql::ast::Cte {
                    name: cte.name.clone(),
                    body: spg_sql::ast::CteBody::Select(body_select.clone()),
                    recursive: true,
                    column_overrides: cte.column_overrides.clone(),
                    search: None,
                    cycle: None,
                };
                if catalog.get(&cte.name).is_some() {
                    let _ = catalog.drop_table(&cte.name);
                }
                self.materialise_recursive_cte(&synthetic, &catalog, cancel)?
            } else {
                let mut cte_engine = Engine::restore(catalog.clone());
                if let Some(c) = self.clock {
                    cte_engine = cte_engine.with_clock(c);
                }
                if let Some(f) = self.salt_fn {
                    cte_engine = cte_engine.with_salt_fn(f);
                }
                let body_result = cte_engine.exec_select_cancel(body_select, cancel)?;
                let QueryResult::Rows { columns, rows } = body_result else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} body did not return rows",
                        cte.name
                    )));
                };
                (columns, rows)
            };
            let inferred = infer_column_types(&columns, &rows);
            let mut columns = inferred;
            if !cte.column_overrides.is_empty() {
                if cte.column_overrides.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} column list has {} names but body returns {} columns",
                        cte.name,
                        cte.column_overrides.len(),
                        columns.len()
                    )));
                }
                for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                    col.name.clone_from(name);
                }
            }
            let schema = TableSchema::new(cte.name.clone(), columns);
            // v7.39 (round 156) — the body ran against the untouched clone;
            // from here on the CTE name resolves to the temp (PG scoping).
            if catalog.get(&cte.name).is_some() {
                let _ = catalog.drop_table(&cte.name);
            }
            catalog.create_table(schema).map_err(EngineError::Storage)?;
            let table = catalog
                .get_mut(&cte.name)
                .expect("just-created CTE table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
            }
        }
        Ok(catalog)
    }

    /// v7.37.43-T4.4 — shared CTE materialiser (mutable variant).
    /// Retained for non-DML callers; the DML path (writable CTE on
    /// INSERT/UPDATE/DELETE outer) uses `run_with_cte_temps` in
    /// `dml.rs` which installs the CTE temps directly on the
    /// active catalog so the outer statement's writes hit real
    /// tables.
    #[allow(dead_code)]
    pub(crate) fn materialise_ctes(
        &mut self,
        ctes: &[spg_sql::ast::Cte],
        cancel: CancelToken<'_>,
    ) -> Result<crate::Catalog, EngineError> {
        cancel.check()?;
        // v7.37.43-T4.4 — modifying CTEs need to write through the
        // SAME catalog as the outer statement, not a clone (PG's
        // writable CTE puts all modifications in one transaction).
        // For the read-only case the original logic cloned, but
        // since the outer statement also goes through the cloned
        // engine and ALL writes must converge, we now drive the
        // accumulator off `self.active_catalog().clone()` and
        // commit the modifying writes directly to `self`'s active
        // catalog so the surface is consistent.
        let mut catalog = self.active_catalog().clone();
        // v7.39 (round 149) — a modifying CTE body's target must be a
        // real relation, never a sibling CTE (PG: relation does not
        // exist); checked before any alias lands in the accumulator.
        for cte in ctes {
            let body_target = match &cte.body {
                spg_sql::ast::CteBody::Select(_) => None,
                spg_sql::ast::CteBody::Insert(i) => Some(i.table.as_str()),
                spg_sql::ast::CteBody::Update(u) => Some(u.table.as_str()),
                spg_sql::ast::CteBody::Delete(d) => Some(d.table.as_str()),
                spg_sql::ast::CteBody::Merge(m) => Some(m.target.as_str()),
            };
            if let Some(t) = body_target
                && ctes.iter().any(|c| c.name.eq_ignore_ascii_case(t))
                && catalog.get(t).is_none()
            {
                return Err(EngineError::Storage(
                    spg_storage::StorageError::TableNotFound { name: t.into() },
                ));
            }
        }
        for cte in ctes {
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let (columns, rows) = match &cte.body {
                // v7.39 (round 145) — see the sibling site: only a body that
                // truly self-references takes the iterating materialiser.
                spg_sql::ast::CteBody::Select(body)
                    if cte.recursive && select_refers_to(body, &cte.name) =>
                {
                    // Recursive CTE — the existing helper takes a
                    // SELECT body and the snapshot catalog.
                    let synthetic = spg_sql::ast::Cte {
                        name: cte.name.clone(),
                        body: spg_sql::ast::CteBody::Select(body.clone()),
                        recursive: true,
                        column_overrides: cte.column_overrides.clone(),
                        search: None,
                        cycle: None,
                    };
                    self.materialise_recursive_cte(&synthetic, &catalog, cancel)?
                }
                spg_sql::ast::CteBody::Select(body) => {
                    // v7.25 (round-17) — run against the accumulated
                    // catalog so later CTEs can reference earlier
                    // ones in the same WITH clause.
                    let mut cte_engine = Engine::restore(catalog.clone());
                    if let Some(c) = self.clock {
                        cte_engine = cte_engine.with_clock(c);
                    }
                    if let Some(f) = self.salt_fn {
                        cte_engine = cte_engine.with_salt_fn(f);
                    }
                    let body_result = cte_engine.exec_select_cancel(body, cancel)?;
                    let QueryResult::Rows { columns, rows } = body_result else {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "CTE {:?} body did not return rows",
                            cte.name
                        )));
                    };
                    (columns, rows)
                }
                spg_sql::ast::CteBody::Insert(body) => {
                    self.exec_modifying_cte_insert(&cte.name, body, cancel)?
                }
                spg_sql::ast::CteBody::Update(body) => {
                    self.exec_modifying_cte_update(&cte.name, body, cancel)?
                }
                spg_sql::ast::CteBody::Delete(body) => {
                    self.exec_modifying_cte_delete(&cte.name, body, cancel)?
                }
                spg_sql::ast::CteBody::Merge(body) => {
                    self.exec_modifying_cte_merge(&cte.name, body, cancel)?
                }
            };
            // v4.22: the projection builder labels any non-column
            // expression as Text — including literal SELECT 1.
            // Promote each column's type to whatever the rows
            // actually carry so the CTE storage table accepts them.
            let inferred = infer_column_types(&columns, &rows);
            let mut columns = inferred;
            if !cte.column_overrides.is_empty() {
                if cte.column_overrides.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} column list has {} names but body returns {} columns",
                        cte.name,
                        cte.column_overrides.len(),
                        columns.len()
                    )));
                }
                for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                    col.name.clone_from(name);
                }
            }
            let schema = TableSchema::new(cte.name.clone(), columns);
            catalog.create_table(schema).map_err(EngineError::Storage)?;
            let table = catalog
                .get_mut(&cte.name)
                .expect("just-created CTE table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
            }
        }
        Ok(catalog)
    }

    /// v7.37.43-T4.4 — execute an INSERT CTE body. Runs the INSERT
    /// against `self` (so the mutation lands in the active catalog
    /// inside the current transaction) and captures the RETURNING
    /// projection — column schema + rows — to materialise as the
    /// CTE alias's table. An INSERT without RETURNING produces a
    /// 0-row table with a synthetic single-column placeholder
    /// (matches PG: the CTE alias is still defined, but referencing
    /// it from the outer query without RETURNING raises a
    /// column-resolution error at scan time).
    fn exec_modifying_cte_insert(
        &mut self,
        cte_name: &str,
        body: &spg_sql::ast::InsertStatement,
        _cancel: CancelToken<'_>,
    ) -> Result<
        (
            Vec<spg_storage::ColumnSchema>,
            Vec<spg_storage::Row<'static>>,
        ),
        EngineError,
    > {
        // round 151 — a WITH-headed body keeps its own ctes; the body
        // statement routes through its writable-CTE entry (outer CTEs
        // are never copied into bodies, so no recursion risk).
        let body = body.clone();
        let result = self.exec_insert(body)?;
        match result {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            QueryResult::CommandOk { .. } => {
                // No RETURNING — emit a sentinel single-column
                // schema with zero rows so the alias is defined.
                let placeholder = spg_storage::ColumnSchema::new(
                    alloc::format!("{cte_name}_returning_absent"),
                    spg_storage::DataType::Text,
                    true,
                );
                Ok((alloc::vec![placeholder], Vec::new()))
            }
        }
    }

    /// v7.37.43-T4.4 — execute an UPDATE CTE body, same semantics
    /// as INSERT above.
    fn exec_modifying_cte_update(
        &mut self,
        cte_name: &str,
        body: &spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
    ) -> Result<
        (
            Vec<spg_storage::ColumnSchema>,
            Vec<spg_storage::Row<'static>>,
        ),
        EngineError,
    > {
        let body = body.clone();
        let result = self.exec_update_cancel(&body, cancel)?;
        match result {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            QueryResult::CommandOk { .. } => {
                let placeholder = spg_storage::ColumnSchema::new(
                    alloc::format!("{cte_name}_returning_absent"),
                    spg_storage::DataType::Text,
                    true,
                );
                Ok((alloc::vec![placeholder], Vec::new()))
            }
        }
    }

    /// v7.37.43-T4.4 — execute a DELETE CTE body.
    fn exec_modifying_cte_delete(
        &mut self,
        cte_name: &str,
        body: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<
        (
            Vec<spg_storage::ColumnSchema>,
            Vec<spg_storage::Row<'static>>,
        ),
        EngineError,
    > {
        let body = body.clone();
        let result = self.exec_delete_cancel(&body, cancel)?;
        match result {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            QueryResult::CommandOk { .. } => {
                let placeholder = spg_storage::ColumnSchema::new(
                    alloc::format!("{cte_name}_returning_absent"),
                    spg_storage::DataType::Text,
                    true,
                );
                Ok((alloc::vec![placeholder], Vec::new()))
            }
        }
    }

    /// v7.39 (round 149) — execute a MERGE CTE body (PG 17).
    fn exec_modifying_cte_merge(
        &mut self,
        cte_name: &str,
        body: &spg_sql::ast::MergeStatement,
        cancel: CancelToken<'_>,
    ) -> Result<
        (
            Vec<spg_storage::ColumnSchema>,
            Vec<spg_storage::Row<'static>>,
        ),
        EngineError,
    > {
        let body = body.clone();
        let result = self.exec_merge_cancel(&body, cancel)?;
        match result {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            QueryResult::CommandOk { .. } => {
                let placeholder = spg_storage::ColumnSchema::new(
                    alloc::format!("{cte_name}_returning_absent"),
                    spg_storage::DataType::Text,
                    true,
                );
                Ok((alloc::vec![placeholder], Vec::new()))
            }
        }
    }

    /// v4.22: materialise a WITH RECURSIVE CTE. The body must be a
    /// UNION (or UNION ALL) of an anchor that does not reference
    /// the CTE name, and one or more recursive terms that do. The
    /// anchor runs first; each subsequent iteration runs the
    /// recursive term against a temp catalog where the CTE name is
    /// bound to the *previous* iteration's output. Iteration stops
    /// when the recursive term yields no rows; UNION (DISTINCT)
    /// deduplicates against the accumulated result, UNION ALL does
    /// not. A hard cap on total rows prevents runaway queries.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn materialise_recursive_cte(
        &self,
        cte: &spg_sql::ast::Cte,
        base_catalog: &Catalog,
        cancel: CancelToken<'_>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row<'static>>), EngineError> {
        const MAX_TOTAL_ROWS: usize = 1_000_000;
        const MAX_ITERATIONS: usize = 100_000;
        cancel.check()?;
        // v7.37.43-T4.4 — RECURSIVE only supports SELECT bodies;
        // a modifying recursive CTE is parser-rejectable but we
        // guard here defensively.
        let body_select = cte.body.as_select().ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?} body must be a SELECT, not a data-modifying statement",
                cte.name
            ))
        })?;
        if body_select.unions.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?} body must be a UNION of an anchor and a recursive term",
                cte.name
            )));
        }
        // Anchor: the body's leading SELECT, with unions stripped.
        let mut anchor = body_select.clone();
        let all_union_terms = core::mem::take(&mut anchor.unions);
        anchor.ctes = Vec::new();
        // v7.37 D.42 — split the UNION members: those that do NOT reference the
        // CTE are additional ANCHOR terms, only the ones that do recurse. A
        // multi-row VALUES seed lowers to `SELECT r1 UNION ALL SELECT r2 UNION
        // ALL <recursive>`, so the leading SELECT alone is not the whole anchor —
        // treating the non-recursive `SELECT r2` as a recursive term made it
        // re-emit its constant row every iteration → runaway loop.
        let (anchor_terms, union_terms): (Vec<_>, Vec<_>) = all_union_terms
            .into_iter()
            .partition(|(_, t)| !select_refers_to(t, &cte.name));
        let anchor_result = self.exec_select_cancel(&anchor, cancel)?;
        let QueryResult::Rows {
            columns: anchor_cols,
            rows: mut anchor_rows,
        } = anchor_result
        else {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: anchor did not return rows",
                cte.name
            )));
        };
        // Append every non-recursive UNION member's rows to the anchor set.
        for (_, term) in &anchor_terms {
            let mut term = term.clone();
            term.ctes = Vec::new();
            if let QueryResult::Rows { rows, .. } = self.exec_select_cancel(&term, cancel)? {
                anchor_rows.extend(rows);
            }
        }
        // The projection builder labels non-column expressions Text;
        // refine column types from the anchor's actual values so the
        // intermediate iter-catalog tables accept them.
        let mut columns = infer_column_types(&anchor_cols, &anchor_rows);
        if !cte.column_overrides.is_empty() {
            if cte.column_overrides.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE {:?} column list has {} names but anchor returns {} columns",
                    cte.name,
                    cte.column_overrides.len(),
                    columns.len()
                )));
            }
            for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                col.name.clone_from(name);
            }
        }
        let mut all_rows: Vec<Row<'static>> = anchor_rows.clone();
        let mut working_set: Vec<Row<'static>> = anchor_rows;
        let mut seen: alloc::collections::BTreeSet<Vec<u8>> = alloc::collections::BTreeSet::new();
        // Track at least one "all UNION ALL" flag — if every union
        // kind is ALL we skip the dedup step (faster + matches PG).
        let all_union_all = union_terms.iter().all(|(k, _)| matches!(k, UnionKind::All));
        if !all_union_all {
            for r in &all_rows {
                seen.insert(encode_row_key(r));
            }
        }
        // v7.39 (round 598) — the engine and its catalog are built ONCE.
        // Each iteration used to clone the catalog, create the CTE table,
        // and construct a whole `Engine` — which initialises 82 fields — to
        // hold that round's working set. A counting allocator put the loop
        // at 63 allocations and 104 kB per iteration, or 1 GB for a
        // 10,000-row recursive CTE, and none of it varied with how much
        // else was in the catalog: the per-round rebuild WAS the cost. The
        // table is emptied and refilled instead.
        let mut iter_catalog = base_catalog.clone();
        let schema = TableSchema::new(cte.name.clone(), columns.clone());
        iter_catalog
            .create_table(schema)
            .map_err(EngineError::Storage)?;
        let mut iter_engine = Engine::restore(iter_catalog);
        if let Some(c) = self.clock {
            iter_engine = iter_engine.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            iter_engine = iter_engine.with_salt_fn(f);
        }
        // The recursive terms are cloned once too — the clone stripped the
        // CTE list off each of them, per term per iteration.
        let recursive_terms: Vec<SelectStatement> = union_terms
            .iter()
            .map(|(_, t)| {
                let mut t = t.clone();
                t.ctes = Vec::new();
                t
            })
            .collect();
        // v7.39 (round 618) — plan every recursive term once. Taken only if
        // ALL of them plan, so a query never runs half on each path.
        let term_plans: Option<Vec<RecursiveTermPlan<'_>>> = recursive_terms
            .iter()
            .map(|t| plan_recursive_term(t, &cte.name, columns.len()))
            .collect();
        let fast_ctx = term_plans.as_ref().map(|plans| {
            let alias = plans[0].alias.clone();
            (alias, ())
        });
        for iter in 0..MAX_ITERATIONS {
            cancel.check()?;
            if working_set.is_empty() {
                break;
            }
            if let (Some(plans), Some((_, ()))) = (term_plans.as_ref(), fast_ctx.as_ref()) {
                // The worktable IS the working set: no table to empty and
                // refill, and no query execution per round.
                let mut next_set: Vec<Row<'static>> = Vec::new();
                for plan in plans {
                    let ctx = self.ev_ctx(&columns, Some(&plan.alias));
                    for row in &working_set {
                        cancel.check()?;
                        if let Some(w) = plan.where_ {
                            let v = eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                            if !matches!(v, Value::Bool(true)) {
                                continue;
                            }
                        }
                        let mut vals: Vec<Value<'static>> = Vec::with_capacity(plan.items.len());
                        for it in &plan.items {
                            vals.push(eval::eval_expr(it, row, &ctx).map_err(EngineError::Eval)?);
                        }
                        let out = Row::new(vals);
                        if !all_union_all {
                            let key = encode_row_key(&out);
                            if !seen.insert(key) {
                                continue;
                            }
                        }
                        next_set.push(out);
                    }
                }
                if next_set.is_empty() {
                    break;
                }
                all_rows.extend(next_set.iter().cloned());
                working_set = next_set;
                if all_rows.len() > MAX_TOTAL_ROWS {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: produced more than {MAX_TOTAL_ROWS} rows — likely runaway recursion",
                        cte.name
                    )));
                }
                if iter + 1 == MAX_ITERATIONS {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: exceeded {MAX_ITERATIONS} iterations",
                        cte.name
                    )));
                }
                continue;
            }
            {
                // Truncated rather than dropped and recreated: the table's
                // own structure is what dropping it throws away, and it is
                // identical every round.
                let cat = iter_engine.base_catalog_mut();
                let table = cat.get_mut(&cte.name).expect("created above");
                table.truncate();
                for row in &working_set {
                    table.insert(row.clone()).map_err(EngineError::Storage)?;
                }
            }
            // Run each recursive term in sequence and collect new rows.
            let mut next_set: Vec<Row<'static>> = Vec::new();
            for term in &recursive_terms {
                let r = iter_engine.exec_select_cancel(term, cancel)?;
                let QueryResult::Rows {
                    columns: rc,
                    rows: rs,
                } = r
                else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: recursive term did not return rows",
                        cte.name
                    )));
                };
                if rc.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: column count of recursive term ({}) does not match anchor ({})",
                        cte.name,
                        rc.len(),
                        columns.len()
                    )));
                }
                for row in rs {
                    if !all_union_all {
                        let key = encode_row_key(&row);
                        if !seen.insert(key) {
                            continue;
                        }
                    }
                    next_set.push(row);
                }
            }
            if next_set.is_empty() {
                break;
            }
            all_rows.extend(next_set.iter().cloned());
            working_set = next_set;
            if all_rows.len() > MAX_TOTAL_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: produced more than {MAX_TOTAL_ROWS} rows — likely runaway recursion",
                    cte.name
                )));
            }
            if iter + 1 == MAX_ITERATIONS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: exceeded {MAX_ITERATIONS} iterations",
                    cte.name
                )));
            }
        }
        Ok((columns, all_rows))
    }

    pub(crate) fn resolve_select_subqueries(
        &self,
        stmt: &mut SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for item in &mut stmt.items {
            if let SelectItem::Expr { expr, alias } = item {
                // An UNCORRELATED subquery is replaced by its value right
                // here, and the shape the column was named for goes with
                // it: by projection time `SELECT EXISTS(SELECT 1)` is a
                // boolean literal, so SPG answered `?column?` where PG18
                // answers `exists`. Only a subquery at the TOP of the item
                // loses its name this way — one nested inside a call still
                // reports the call.
                if alias.is_none()
                    && matches!(
                        expr,
                        Expr::ScalarSubquery(_)
                            | Expr::Exists { .. }
                            | Expr::InSubquery { .. }
                            | Expr::RowInSubquery { .. }
                            | Expr::RowCmpSubquery { .. }
                    )
                {
                    *alias = Some(default_output_name(expr, self.backslash_escapes));
                }
                self.resolve_expr_subqueries(expr, cancel)?;
            }
        }
        if let Some(w) = &mut stmt.where_ {
            self.resolve_expr_subqueries(w, cancel)?;
        }
        // v7.24.1 — JOIN ON conditions can carry subqueries too;
        // they were never walked, so even an UNCORRELATED subquery
        // in ON hit "subquery reached row eval".
        if let Some(from) = &mut stmt.from {
            for j in &mut from.joins {
                if let Some(on) = &mut j.on {
                    self.resolve_expr_subqueries(on, cancel)?;
                }
            }
        }
        if let Some(gs) = &mut stmt.group_by {
            for g in gs {
                self.resolve_expr_subqueries(g, cancel)?;
            }
        }
        if let Some(h) = &mut stmt.having {
            self.resolve_expr_subqueries(h, cancel)?;
        }
        for o in &mut stmt.order_by {
            self.resolve_expr_subqueries(&mut o.expr, cancel)?;
        }
        for (_, peer) in &mut stmt.unions {
            self.resolve_select_subqueries(peer, cancel)?;
        }
        Ok(())
    }

    #[allow(clippy::only_used_in_recursion)] // engine handle reads aren't really pure
    pub(crate) fn resolve_expr_subqueries(
        &self,
        e: &mut Expr,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        // Replace-on-this-node cases first.
        if let Some(replacement) = self.subquery_replacement(e, cancel)? {
            *e = replacement;
            return Ok(());
        }
        match e {
            Expr::NamedArg { expr, .. } => self.resolve_expr_subqueries(expr, cancel)?,
            Expr::Variadic(expr) => self.resolve_expr_subqueries(expr, cancel)?,
            Expr::AggregateOrdered { call, order_by, .. } => {
                self.resolve_expr_subqueries(call, cancel)?;
                for o in order_by.iter_mut() {
                    self.resolve_expr_subqueries(&mut o.expr, cancel)?;
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr_subqueries(lhs, cancel)?;
                self.resolve_expr_subqueries(rhs, cancel)?;
            }
            Expr::Unary { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::BoolTest { expr, .. }
            | Expr::FieldAccess { base: expr, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                self.resolve_expr_subqueries(pattern, cancel)?;
            }
            Expr::Extract { source, .. } => self.resolve_expr_subqueries(source, cancel)?,
            // v4.12 window functions — recurse into args + ORDER BY
            // + PARTITION BY in case they carry inner subqueries.
            Expr::WindowFunction {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
                for p in partition_by {
                    self.resolve_expr_subqueries(p, cancel)?;
                }
                for (e, _, _) in order_by {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
            }
            // Subquery nodes are handled in subquery_replacement
            // (which returned None — defensive no-op); Literal /
            // Column are leaves.
            Expr::ScalarSubquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::RowInSubquery { .. }
            | Expr::RowCmpSubquery { .. }
            | Expr::Literal(_)
            | Expr::Placeholder(_)
            | Expr::Column(_) => {}
            // v7.30.2 — list elements can carry scalar subqueries
            // (`x IN (1, (SELECT …))`).
            Expr::InList { expr, list, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                for item in list {
                    self.resolve_expr_subqueries(item, cancel)?;
                }
            }
            // v7.10.10 — recurse children.
            Expr::Array(items) => {
                for elem in items {
                    self.resolve_expr_subqueries(elem, cancel)?;
                }
            }
            Expr::ArraySubscript { target, index } => {
                self.resolve_expr_subqueries(target, cancel)?;
                self.resolve_expr_subqueries(index, cancel)?;
            }
            Expr::ArraySlice { target, lo, hi } => {
                self.resolve_expr_subqueries(target, cancel)?;
                if let Some(l) = lo {
                    self.resolve_expr_subqueries(l, cancel)?;
                }
                if let Some(h) = hi {
                    self.resolve_expr_subqueries(h, cancel)?;
                }
            }
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                // Quantified subquery — an uncorrelated one
                // materialises up front; a correlated one stays for
                // the per-row resolver.
                if let Expr::ScalarSubquery(inner) = array.as_mut() {
                    if !crate::subquery::select_is_correlated(inner) {
                        let s = (**inner).clone();
                        **array = self.materialize_quantified_rows(&s, cancel)?;
                    }
                } else {
                    self.resolve_expr_subqueries(array, cancel)?;
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.resolve_expr_subqueries(o, cancel)?;
                }
                for (w, t) in branches {
                    self.resolve_expr_subqueries(w, cancel)?;
                    self.resolve_expr_subqueries(t, cancel)?;
                }
                if let Some(e) = else_branch {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
            }
        }
        Ok(())
    }
}

impl Engine {
    /// v6.10.2 — projection for AS OF SEGMENT. Resolves
    /// `SelectItem::Wildcard` to all schema columns and
    /// `SelectItem::Expr` via the regular eval path.
    pub(crate) fn project_row_simple(
        &self,
        row: &Row<'static>,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        alias: &str,
    ) -> Result<Row<'static>, EngineError> {
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let cancel = CancelToken::none();
        let mut out_vals = Vec::new();
        for item in items {
            match item {
                // In a single-table projection (AS OF SEGMENT / RETURNING) a
                // qualified `t.*` covers exactly the same columns as a bare `*`.
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    out_vals.extend(row.values.iter().cloned());
                }
                SelectItem::Expr { expr, .. } => {
                    let v = self.eval_expr_with_correlated(expr, row, &ctx, cancel, None)?;
                    out_vals.push(v);
                }
            }
        }
        Ok(Row::new(out_vals))
    }

    /// v6.10.2 — derive the output `ColumnSchema` list for an
    /// AS OF SEGMENT projection. Wildcards take the full schema;
    /// expressions take the alias if present or a synthetic
    /// `?column?` (PG convention) otherwise.
    pub(crate) fn derive_output_columns(
        &self,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        table_alias: &str,
    ) -> Vec<ColumnSchema> {
        let mut out = Vec::new();
        for item in items {
            match item {
                // `t.*` / `OLD.*` / `NEW.*` all mirror the full table schema in
                // a single-table projection.
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    out.extend(schema_cols.iter().cloned());
                }
                SelectItem::Expr { expr, alias } => {
                    // Bare column references inherit the schema
                    // column's name + type — PG names `RETURNING id`
                    // "id" and types it BIGINT, and the sqlx embed
                    // path type-checks RowDescription against the
                    // Rust target (mailrs embed round-12).
                    if let Expr::Column(col) = expr
                        && let Some(sc) = schema_cols.iter().find(|c| c.name == col.name)
                    {
                        let name = alias.clone().unwrap_or_else(|| sc.name.clone());
                        let mut c = ColumnSchema::new(name, sc.ty, sc.nullable);
                        // v7.39 (read01 round 54) — carry the enum identity:
                        // it lives outside the DataType lattice, so a derived
                        // table built from this schema otherwise forgets it and
                        // the OUTER `ORDER BY <enum col>` silently sorts by the
                        // label's TEXT instead of member order.
                        c.user_enum_type = sc.user_enum_type.clone();
                        out.push(c);
                        continue;
                    }
                    let name = alias.clone().unwrap_or_else(|| "?column?".to_string());
                    // v7.30.4 (mailrs round-27, P0) — type the
                    // expression with the same inference the SELECT
                    // list uses (INT−INT=INT, BIGINT+INT=BIGINT…).
                    // The old Text default broke every typed decode
                    // of `RETURNING uidnext - 1 AS uid`: four days
                    // of inbound mail indexed nowhere. Inference
                    // failure keeps the old Text fallback rather
                    // than inventing new error paths here.
                    // v7.39 (round 258) — take the enum identity from the
                    // same projection build, not just the type: a constant
                    // SELECT (`SELECT 'ok'::mood AS x`, which is what a
                    // VALUES row lowers to) is an EXPRESSION, so it landed
                    // here and the derived table forgot the enum.
                    let (ty, nullable) = build_projection(
                        core::slice::from_ref(item),
                        schema_cols,
                        table_alias,
                        self.backslash_escapes,
                    )
                    .ok()
                    .and_then(|p| p.into_iter().next())
                    .map_or((DataType::Text, true), |p| (p.ty, p.nullable));
                    out.push(ColumnSchema::new(name, ty, nullable));
                }
            }
        }
        out
    }

    /// v4.5: SELECT with cooperative cancellation. The token is
    /// honoured between UNION peers and inside the bare-SELECT row
    /// loop; HNSW kNN graph walks and the aggregate executor don't
    /// honour it yet (deferred — those paths bound their work
    /// internally by `LIMIT k` and `GROUP BY` cardinality).
    /// v7.38 (read01 P3.NEW3) — materialise a `spg_*` / `pg_*` meta-view by
    /// its (lowercased) name, or None if the name isn't a virtual view.
    /// Callers decide whether to return it directly (`SELECT *`) or stage
    /// it as a temp table for the full query pipeline.
    fn meta_view_result(&self, name: &str) -> Option<QueryResult> {
        Some(match name {
            "spg_statistic" => self.exec_spg_statistic(),
            "spg_stat_replication" => self.exec_spg_stat_replication(),
            "spg_stat_segment" => self.exec_spg_stat_segment(),
            "spg_memory_stats" => self.exec_spg_memory_stats(),
            "spg_stat_query" => self.exec_spg_stat_query(),
            "pg_stat_statements" => self.exec_pg_stat_statements(),
            "spg_stat_activity" => self.exec_spg_stat_activity(),
            "pg_stat_activity" => self.exec_pg_stat_activity(),
            "pg_locks" => self.exec_pg_locks(),
            "pg_statio_user_tables" => self.exec_pg_statio_user_tables(),
            "spg_stat_mvcc" => self.exec_spg_stat_mvcc(),
            "spg_partition_health" => self.exec_spg_partition_health(),
            "spg_audit_chain" => self.exec_spg_audit_chain(),
            "spg_audit_verify" => self.exec_spg_audit_verify(),
            "spg_table_ddl" => self.exec_spg_table_ddl(),
            "spg_role_ddl" => self.exec_spg_role_ddl(),
            "spg_database_ddl" => self.exec_spg_database_ddl(),
            _ => return None,
        })
    }

    /// v7.39 (round 462) — the catalog an admin / stat view SELECT
    /// describes against: this engine's catalog with the view staged as a
    /// table, exactly as `exec_select_cancel_as` stages it for a
    /// non-bare query.
    ///
    /// These views never reach the catalog — each is a fixed row set built
    /// inside its own `exec_*` — so Describe reported no columns for all
    /// seventeen of them. Rows are deliberately not inserted: Describe
    /// only needs the shape, and `infer_column_types` reads the rows we
    /// already have in hand.
    pub(crate) fn admin_view_catalog(&self, stmt: &SelectStatement) -> Option<Catalog> {
        let from = stmt.from.as_ref()?;
        if !from.joins.is_empty() || self.active_catalog().get(&from.primary.name).is_some() {
            return None;
        }
        let lower = from.primary.name.to_ascii_lowercase();
        let QueryResult::Rows { columns, rows } = self.meta_view_result(&lower)? else {
            return None;
        };
        let mut catalog = self.active_catalog().clone();
        let cols = infer_column_types(&columns, &rows);
        catalog
            .create_table(TableSchema::new(from.primary.name.clone(), cols))
            .ok()?;
        Some(catalog)
    }

    pub(crate) fn exec_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.exec_select_cancel_as(stmt, cancel, None)
    }

    /// v7.39 (round 334, V55) — the same read core, authorised as
    /// `as_role`. A `SECURITY DEFINER` function's body runs as the
    /// function's OWNER: that is the entire point of the form, and without
    /// it every definer function failed with "permission denied" on the
    /// very table it exists to expose.
    /// v7.39 (round 559) — see the call site. `None` for anything but
    /// the bare shape, so every other query keeps its old path.
    fn try_bare_count_star(
        &self,
        stmt: &SelectStatement,
        as_role: Option<&str>,
    ) -> Result<Option<QueryResult>, EngineError> {
        use spg_sql::ast::SelectItem;
        if as_role.is_some()
            || !stmt.ctes.is_empty()
            || !stmt.unions.is_empty()
            || stmt.where_.is_some()
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || stmt.distinct
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.items.len() != 1
        {
            return Ok(None);
        }
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        if !from.joins.is_empty()
            || stmt.locking.is_some()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.name.is_empty()
            || from.primary.name.starts_with("__spg_")
        {
            return Ok(None);
        }
        // A partition PARENT holds no rows of its own — they live in the
        // children — so its header count is 0 and the ordinary path has
        // to fan out. Caught by the partition conformance cases.
        //
        // v7.39 (round 645) — and an INHERITANCE parent holds only SOME
        // of them, which is worse: its header count is a real number,
        // just not the answer. `SELECT count(*) FROM par` returned 1
        // where PG returns 2, because this shortcut fired before the
        // fan-out could. The question is "does anything descend from
        // this", not "was it declared a partition parent".
        if crate::partition::has_children(self.active_catalog(), &from.primary.name) {
            return Ok(None);
        }
        let SelectItem::Expr { expr, alias } = &stmt.items[0] else {
            return Ok(None);
        };
        let spg_sql::ast::Expr::FunctionCall { name, args } = expr else {
            return Ok(None);
        };
        if !name.eq_ignore_ascii_case("count_star") || !args.is_empty() {
            return Ok(None);
        }
        // A row-security policy filters rows, so the header count is not
        // the answer; the ordinary path applies the policy.
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        if table.schema().row_security {
            return Ok(None);
        }
        // Rows frozen to the cold tier are not in `headers`, so the
        // header count would miss them. Caught by the cold-tier e2e.
        if table.has_cold_rows_fast() {
            return Ok(None);
        }
        let n = table.count_visible(&self.current_snapshot());
        let col = alias.clone().unwrap_or_else(|| String::from("count"));
        Ok(Some(QueryResult::Rows {
            columns: alloc::vec![ColumnSchema::new(col, DataType::BigInt, false)],
            rows: alloc::vec![Row::new(alloc::vec![Value::BigInt(
                i64::try_from(n).unwrap_or(i64::MAX)
            )])],
        }))
    }

    /// v7.39 (round 560) — `SELECT <indexed col> FROM t WHERE <range on
    /// that col>` served from the index, never reading a row.
    ///
    /// Measured over pgwire on a 500k table, a 100k-row range: PG18's
    /// Index Only Scan 3.6 ms against SPG's 30 ms, widening with the row
    /// count (2x at 1k). PG needs its visibility map for this — a heap
    /// tuple carries its own visibility, so an index entry alone cannot
    /// say whether the row is live, and PG reads the heap for any page
    /// the map does not mark all-visible. SPG keeps a header array
    /// beside the rows, so the locator answers it directly and there is
    /// no map to be stale.
    /// v7.39 (round 564) — the shape test, once, for both the
    /// materialising scan and the streaming one.
    ///
    /// Two callers asking the same question in two places is how a fact
    /// starts drifting; the answer here is the single copy. Returns the
    /// table, the alias the predicate is written against, the projected
    /// column's position, and the name the single output column takes.
    pub(crate) fn index_only_shape<'s>(
        &'s self,
        stmt: &'s SelectStatement,
    ) -> Option<(&'s spg_storage::Table, &'s str, usize, String)> {
        use spg_sql::ast::SelectItem;
        if !stmt.ctes.is_empty()
            || !stmt.unions.is_empty()
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || stmt.distinct
            || stmt.locking.is_some()
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.items.len() != 1
        {
            return None;
        }
        let (Some(from), Some(_)) = (&stmt.from, &stmt.where_) else {
            return None;
        };
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.name.is_empty()
            || from.primary.name.starts_with("__spg_")
        {
            return None;
        }
        // v7.39 (round 645) — see the note on the sibling shortcut above:
        // an inheritance parent's own header count is not the answer.
        if crate::partition::has_children(self.active_catalog(), &from.primary.name) {
            return None;
        }
        let SelectItem::Expr { expr, alias } = &stmt.items[0] else {
            return None;
        };
        let spg_sql::ast::Expr::Column(c) = expr else {
            return None;
        };
        let alias_name = from.primary.alias.as_deref().unwrap_or(&from.primary.name);
        if let Some(q) = c.qualifier.as_deref()
            && !q.eq_ignore_ascii_case(alias_name)
        {
            return None;
        }
        let table = self.active_catalog().get(&from.primary.name)?;
        if table.schema().row_security {
            return None;
        }
        let cols = &table.schema().columns;
        let pos = cols
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&c.name))?;
        let out = alias.clone().unwrap_or_else(|| cols[pos].name.clone());
        Some((table, alias_name, pos, out))
    }

    /// v7.39 (round 565) — would this statement be answered out of the
    /// index alone?
    ///
    /// EXPLAIN has to name the node the executor will actually run, and
    /// the only honest way to know is to ask the same two questions the
    /// executor asks: the statement's shape, and everything decidable
    /// about the scan before it walks. Neither is re-stated here.
    pub(crate) fn stmt_takes_index_only_scan(&self, stmt: &SelectStatement) -> bool {
        let Some((table, alias_name, pos, _)) = self.index_only_shape(stmt) else {
            return false;
        };
        let Some(where_) = stmt.where_.as_ref() else {
            return false;
        };
        crate::index_access::index_only_precheck(
            where_,
            &table.schema().columns,
            table,
            alias_name,
            pos,
        )
        .is_some()
    }

    fn try_index_only_scan(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<QueryResult>, EngineError> {
        let Some((table, alias_name, pos, out_name)) = self.index_only_shape(stmt) else {
            return Ok(None);
        };
        let where_ = stmt.where_.as_ref().expect("shape checked it");
        let cols = &table.schema().columns;
        let Some(values) = crate::index_access::try_index_only_range(
            where_,
            cols,
            table,
            alias_name,
            &self.current_snapshot(),
            pos,
        ) else {
            return Ok(None);
        };
        let schema = alloc::vec![ColumnSchema::new(
            out_name,
            cols[pos].ty,
            cols[pos].nullable
        )];
        Ok(Some(QueryResult::Rows {
            columns: schema,
            rows: values
                .into_iter()
                .map(|v| Row::new(alloc::vec![v]))
                .collect(),
        }))
    }

    /// v7.39 (round 564) — the same scan, emitting each value instead of
    /// building a `Vec<Row>` for the encoder to walk once and drop.
    ///
    /// A profile of the server serving a 50k-row range put 10.2% of the
    /// connection thread's CPU on BUILDING that vector and another 9.7%
    /// on dropping it — a fifth of the query, spent allocating and
    /// freeing one single-element `Vec` per output row so that the wire
    /// encoder could borrow each value for a few nanoseconds. The
    /// streaming interface it then hands them to takes `&[Value]`
    /// already.
    ///
    /// Returns `None` when the shape does not apply, so the caller falls
    /// back before anything has been emitted.
    pub(crate) fn try_index_only_stream<F>(
        &self,
        stmt: &SelectStatement,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        let Some((table, alias_name, pos, out_name)) = self.index_only_shape(stmt) else {
            return Ok(None);
        };
        let where_ = stmt.where_.as_ref().expect("shape checked it");
        let cols = &table.schema().columns;
        let schema = alloc::vec![ColumnSchema::new(
            out_name,
            cols[pos].ty,
            cols[pos].nullable
        )];
        let snapshot = self.current_snapshot();
        // The header goes out only once the walk has agreed to run — a
        // shape rejection after it would leave the client with a
        // RowDescription for a result that never comes.
        let mut wrote_header = false;
        let counted = crate::index_access::index_only_range_each(
            where_,
            cols,
            table,
            alias_name,
            &snapshot,
            pos,
            &mut |v: spg_storage::Value<'_>| {
                if !wrote_header {
                    emit(crate::StreamItem::Header(&schema))?;
                    wrote_header = true;
                }
                emit(crate::StreamItem::Row(crate::RowCells::Refs(&[&v])))
            },
        );
        match counted {
            None => Ok(None),
            Some(Err(e)) => Err(e),
            Some(Ok(n)) => {
                if !wrote_header {
                    emit(crate::StreamItem::Header(&schema))?;
                }
                Ok(Some(n))
            }
        }
    }

    /// `DISTINCT ON`'s de-duplication, which runs after the inner
    /// SELECT has produced its rows.
    ///
    /// `#[inline(never)]` and out of `exec_select_cancel_as` for the
    /// reason round 848 established: a debug build gives every branch's
    /// locals a slot in the frame whichever branch runs, and this one is
    /// eighty lines of hashing, key slicing and survivor sorting that a
    /// statement without `DISTINCT ON` never touches. Round 867
    /// measured `exec_select_cancel_as` holding ~46 KB on a path that
    /// reaches none of it — the segment that had been blamed on
    /// `exec_bare_select_cancel`, which turned out to hold 2 KB.
    #[inline(never)]
    fn apply_distinct_on(
        &self,
        result: QueryResult,
        don_hidden: usize,
        don_limit: &(
            Option<spg_sql::ast::LimitExpr>,
            Option<spg_sql::ast::LimitExpr>,
        ),
        don_top1: usize,
        orig_order_by: &[spg_sql::ast::OrderBy],
    ) -> Result<QueryResult, EngineError> {
        let QueryResult::Rows { columns, rows } = result else {
            return Ok(result);
        };
        // The keys are the hidden trailing columns appended above.
        // v7.39 (round 729) — top-1 mode: the trailing columns are the
        // DON keys plus the ORDER tail; keep each group's best in one
        // hash pass, then sort the SURVIVORS with the original spec.
        let mut kept: alloc::vec::Vec<Row<'static>>;
        let key_start;
        if don_top1 > 0 {
            let tail = don_top1 - 1;
            key_start = columns.len().saturating_sub(don_hidden + tail);
            let ord_start = key_start + don_hidden;
            let tail_dirs: alloc::vec::Vec<(bool, Option<bool>)> = orig_order_by[don_hidden..]
                .iter()
                .map(|o| (o.desc, o.nulls_first))
                .collect();
            let mysql = self.backslash_escapes;
            let better = |a: &Row<'static>, b: &Row<'static>| -> bool {
                for (k, (desc, nf)) in tail_dirs.iter().enumerate() {
                    let av = a.values.get(ord_start + k).unwrap_or(&Value::Null);
                    let bv = b.values.get(ord_start + k).unwrap_or(&Value::Null);
                    match crate::order_by_value_cmp_in(*desc, *nf, av, bv, mysql) {
                        core::cmp::Ordering::Less => return true,
                        core::cmp::Ordering::Greater => return false,
                        core::cmp::Ordering::Equal => {}
                    }
                }
                false
            };
            let mut slot: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
            let mut best: alloc::vec::Vec<Row<'static>> = alloc::vec::Vec::new();
            let mut keybuf = String::new();
            for row in rows {
                keybuf.clear();
                for v in row.values.get(key_start..ord_start).unwrap_or(&[]) {
                    aggregate::push_canonical_key(&mut keybuf, v);
                }
                match slot.get(keybuf.as_str()) {
                    Some(&i) => {
                        if better(&row, &best[i]) {
                            best[i] = row;
                        }
                    }
                    None => {
                        slot.insert(keybuf.clone(), best.len());
                        best.push(row);
                    }
                }
            }
            // Survivors sort with the FULL original spec (keys are still
            // aboard as hidden columns).
            let full_dirs: alloc::vec::Vec<(bool, Option<bool>)> = orig_order_by
                .iter()
                .map(|o| (o.desc, o.nulls_first))
                .collect();
            best.sort_by(|a, b| {
                for (k, (desc, nf)) in full_dirs.iter().enumerate() {
                    let av = a.values.get(key_start + k).unwrap_or(&Value::Null);
                    let bv = b.values.get(key_start + k).unwrap_or(&Value::Null);
                    match crate::order_by_value_cmp_in(*desc, *nf, av, bv, mysql) {
                        core::cmp::Ordering::Equal => {}
                        o => return o,
                    }
                }
                core::cmp::Ordering::Equal
            });
            for r in &mut best {
                r.values.truncate(key_start);
            }
            kept = best;
        } else {
            key_start = columns.len().saturating_sub(don_hidden);
            let mut seen: alloc::vec::Vec<alloc::vec::Vec<Value<'static>>> = alloc::vec::Vec::new();
            kept = alloc::vec::Vec::new();
            for mut row in rows {
                let key: alloc::vec::Vec<Value<'static>> =
                    row.values.get(key_start..).unwrap_or(&[]).to_vec();
                if seen.iter().any(|k| k == &key) {
                    continue;
                }
                seen.push(key);
                row.values.truncate(key_start);
                kept.push(row);
            }
        }
        let mut columns = columns;
        columns.truncate(key_start);
        // PG limits what DISTINCT ON left, not what fed it.
        let kept = apply_deferred_limit(kept, don_limit);
        Ok(QueryResult::Rows {
            columns,
            rows: kept,
        })
    }

    pub(crate) fn exec_select_cancel_as(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
        as_role: Option<&str>,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 763, F31-C1) — `SELECT *, count(*) … GROUP BY
        // <all columns>` is legal PG (the wildcard expands to grouped
        // columns); SPG refused the whole shape. Expand the wildcard
        // into explicit column refs up front — the aggregate layer's
        // existing "must appear in the GROUP BY clause" validation
        // then answers PG's sentence for any non-grouped column.
        if let Some(expanded) = self.expand_aggregate_wildcard(stmt) {
            return self.exec_select_cancel_as(&expanded, cancel, as_role);
        }
        // v7.39 (round 559) — `SELECT count(*) FROM t` without touching
        // a row.
        //
        // The aggregate layer already short-circuits this to
        // `rows.len()`, so the O(1) part was never the problem — the
        // cost is UPSTREAM, materialising every visible row so that
        // layer can take its length. Measured over pgwire on 500k rows:
        // PG18 8.2 ms with two parallel workers, 10.3 ms with
        // parallelism off, SPG 16.5 ms — 1.6x slower than a
        // single-threaded PG on the commonest aggregate there is, and no
        // ledger entry recorded it.
        //
        // Counting visible HEADERS needs no row at all. PG cannot do
        // this: its visibility lives in the heap tuples themselves, so
        // it has to read them (that is why its own count(*) is a full
        // scan, parallel or not).
        // v7.39 (read01 round 57) — the table-privilege gate on the common
        // read core. A superuser session returns from it immediately.
        // v7.39 (round 529) — resolve an ORDER BY that names an output
        // ALIAS. The statement-level pass never reached a SELECT nested in
        // a FROM clause, a CTE or a scalar subquery, so the same query
        // worked on its own and failed the moment anything wrapped it —
        // which is what generated SQL does constantly.
        let aliased;
        let stmt = if crate::orderby::order_by_names_an_alias(stmt) {
            let mut s = stmt.clone();
            crate::orderby::resolve_order_by_position(&mut s);
            aliased = s;
            &aliased
        } else {
            stmt
        };
        // v7.39 (round 529) — DISTINCT ON needs two things it did not have.
        //
        // Its keys were evaluated against the PROJECTED row, so a key that
        // is not in the select list — `SELECT DISTINCT ON (g) v FROM t
        // ORDER BY g, v DESC`, the canonical "latest row per group" — could
        // not be read at all and the query failed. PG evaluates them on the
        // input. They are projected as hidden columns here and stripped
        // again below, the same way the grouping-set ordering columns
        // already travel.
        //
        // And the dedup ran AFTER the inner statement's LIMIT, so
        // `… DISTINCT ON (g) … LIMIT 2` on four rows answered ONE row where
        // PG answers two: the limit had already taken two rows of the same
        // group before anything deduplicated them. A paginated DISTINCT ON
        // returned short pages, with no error. The limit is deferred to
        // after the dedup, which is PG's order.
        let don_stmt;
        // v7.39 (round 729) — the top-1 consumer needs the ORIGINAL
        // order spec (the rewritten stmt's is emptied).
        let orig_order_by = stmt.order_by.clone();
        let (stmt, don_hidden, don_limit, don_top1) = if stmt.distinct_on.is_empty() {
            (stmt, 0, (None, None), 0usize)
        } else {
            let mut s = stmt.clone();
            let hidden = s.distinct_on.len();
            for (i, e) in stmt.distinct_on.iter().enumerate() {
                s.items.push(SelectItem::Expr {
                    expr: e.clone(),
                    alias: Some(alloc::format!("__distinct_on_{i}")),
                });
            }
            // v7.39 (round 729) — group-top-1 short circuit. When the
            // DISTINCT ON keys are exactly the ORDER BY's leading keys,
            // the answer is "per group, the row that wins the remaining
            // order" — a single O(n) hash pass. The old path sorted the
            // ENTIRE input first (500k rows, ~180 ms on the panel cell)
            // to keep 100. The inner query runs UNSORTED with every
            // order key appended as a hidden column; the dedup below
            // keeps each group's best, then sorts the SURVIVORS.
            // Declared-collation order keys stay on the sorting path
            // (the value comparator here is collation-blind).
            let prefix_matches = s.order_by.len() >= hidden
                && stmt
                    .distinct_on
                    .iter()
                    .zip(s.order_by.iter())
                    .all(|(d, o)| *d == o.expr && !o.desc && o.nulls_first.is_none());
            let colls_plain =
                crate::orderby::order_by_collations(&s.order_by, &self.ev_ctx(&[], None))
                    .map(|cs| cs.iter().all(Option::is_none))
                    .unwrap_or(false);
            let top1_tail = if prefix_matches && colls_plain && s.group_by.is_none() {
                let tail = s.order_by.len() - hidden;
                for (j, o) in s.order_by[hidden..].iter().enumerate() {
                    s.items.push(SelectItem::Expr {
                        expr: o.expr.clone(),
                        alias: Some(alloc::format!("__don_ord_{j}")),
                    });
                }
                // Carry the tail's direction flags through the aliases'
                // ORDER; the survivors re-sort below with the full spec.
                s.order_by = Vec::new();
                tail + 1 // sentinel: 1 + number of tail keys (0 tail is still active)
            } else {
                0
            };
            // Only a folded literal is deferred; a placeholder or an
            // expression keeps the path it has today rather than being
            // resolved a second way here.
            let deferrable = matches!(
                (&s.limit, &s.offset),
                (
                    None | Some(spg_sql::ast::LimitExpr::Literal(_)),
                    None | Some(spg_sql::ast::LimitExpr::Literal(_))
                )
            );
            let deferred = if deferrable {
                (s.limit.take(), s.offset.take())
            } else {
                (None, None)
            };
            don_stmt = s;
            (&don_stmt, hidden, deferred, top1_tail)
        };
        self.acl_check_select_as(stmt, as_role)?;
        validate_aggregate_placement(stmt)?;
        // v7.39 (round 559) — the bare `count(*)` fast path, AFTER the
        // privilege gate above. Placed before it at first, and the
        // security-definer e2e caught it immediately: a SECURITY INVOKER
        // function whose body is `SELECT count(*) FROM t` answered
        // instead of being refused, because the fast path never reached
        // the check.
        if let Some(r) = self.try_bare_count_star(stmt, as_role)? {
            return Ok(r);
        }
        // v7.39 (round 560) — an index-only range scan. Same placement
        // reasoning as the count above: after the privilege gate.
        if let Some(r) = self.try_index_only_scan(stmt)? {
            return Ok(r);
        }
        validate_locking_clause(stmt)?;
        let result = self.exec_select_cancel_inner(stmt, cancel)?;
        // v7.39 (round 135) — drop the synthetic `__grp_ord_*` ordering columns
        // the parser injects for GROUPING() in ORDER BY on a grouping-set query.
        // They carry the per-branch mask through the UNION-ALL sort and must not
        // appear in the output. Stripped per SELECT level (grouping-set queries
        // are often wrapped in a derived subquery), before DISTINCT ON.
        let result = strip_synthetic_order_cols(result);
        // v7.37.17 (17.6 siblings) — `SELECT DISTINCT ON (exprs)`:
        // rows arrive here already ORDER BY'd; keep the FIRST row of
        // each group the expressions define (PG semantics). The
        // expressions evaluate against the projected schema — an
        // expression that isn't in the select list errors honestly.
        if stmt.distinct_on.is_empty() {
            return Ok(result);
        }
        self.apply_distinct_on(result, don_hidden, &don_limit, don_top1, &orig_order_by)
    }

    /// The UNION chain: execute the head as a bare block, then fold each
    /// peer in with left-associative dedup.
    ///
    /// `#[inline(never)]` and out of `exec_select_cancel_inner` for the
    /// reason round 848 established. A statement with no unions returns
    /// one line above the call — and every nested subquery on a deep
    /// path is such a statement, so each level of the recursion carried
    /// 170 lines of locals it could not reach. Round 867 measured that
    /// frame at 34,800 bytes, the largest single one on the descent,
    /// after two earlier attributions had blamed its caller and then its
    /// callee: the gap between two marks is the frame of everything
    /// BETWEEN them, and this function had no mark of its own.
    #[inline(never)]
    fn exec_union_chain(
        &self,
        stmt_ref: &SelectStatement,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // UNION path: clone-strip the head into a bare block (its own
        // DISTINCT and any inner ORDER BY are dropped by parser rule —
        // the wrapper SelectStatement carries them), execute, then chain
        // peers with left-associative dedup semantics.
        // v7.39 (round 232) — the wrapper's ORDER BY addresses the head's
        // output columns; a position past their count is PG's 42P10.
        crate::orderby::check_order_by_positions(stmt_ref)?;
        let mut head_unknown = branch_unknown_mask(stmt_ref);
        let mut head = stmt_ref.clone();
        head.unions = Vec::new();
        head.order_by = Vec::new();
        head.limit = None;
        let QueryResult::Rows {
            mut columns,
            mut rows,
        } = self.exec_bare_select_cancel(&head, cancel)?
        else {
            unreachable!("bare SELECT cannot return CommandOk")
        };
        for (kind, peer) in &stmt_ref.unions {
            // v7.37.17 (17.6 siblings) — a peer carrying its own
            // unions is a nested INTERSECT group (the parser's
            // precedence regrouping); recurse through the
            // union-aware wrapper for it.
            let peer_result = if peer.unions.is_empty() {
                self.exec_bare_select_cancel(peer, cancel)?
            } else {
                self.exec_select_cancel(peer, cancel)?
            };
            let QueryResult::Rows {
                columns: peer_cols,
                rows: mut peer_rows,
            } = peer_result
            else {
                unreachable!("bare SELECT cannot return CommandOk")
            };
            if peer_cols.len() != columns.len() {
                // v7.39 (round 232) — PG's wording, which clients match on.
                return Err(EngineError::Unsupported(alloc::format!(
                    "each {} query must have the same number of columns",
                    set_op_name(*kind)
                )));
            }
            // v7.39 (round 232+233) — PG resolves each result column to one
            // type before it merges anything, and refuses the query when the
            // two branches have no common type. SPG's unifier
            // (`unify_union_columns`) is value-driven and deliberately
            // conservative — "a column where any cell fails to coerce is left
            // exactly as it was" — so a mismatch produced a column holding
            // BOTH types (`SELECT a, b FROM t UNION SELECT b, a FROM t` came
            // back with integers and text interleaved) instead of an error.
            //
            // The check has to read the branch ASTs, not just their schemas:
            // SPG has no `Unknown` DataType, so a bare `'a'` literal describes
            // as TEXT and is indistinguishable from a real text column by
            // schema alone — yet PG treats the two completely differently
            // (`SELECT 1 UNION SELECT 'a'` is an input-syntax error on the
            // literal, `SELECT 1 UNION SELECT 'a'::text` is a type mismatch).
            let peer_unknown = branch_unknown_mask(peer);
            for i in 0..columns.len() {
                let hu = head_unknown.get(i).copied().unwrap_or(false);
                let pu = peer_unknown.get(i).copied().unwrap_or(false);
                let (ht, pt) = (columns[i].ty, peer_cols[i].ty);
                match (hu, pu) {
                    // Both sides carry a real type: they must share a category.
                    (false, false) => {
                        if !crate::conversions::types_unify(ht, pt) {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "{} types {} and {} cannot be matched",
                                set_op_name(*kind),
                                crate::conversions::pg_type_name_for_error(ht),
                                crate::conversions::pg_type_name_for_error(pt),
                            )));
                        }
                    }
                    // One side is an untyped literal: it takes the other's
                    // type, and failing to convert is the error PG reports.
                    (true, false) => {
                        coerce_branch_column(&mut rows, i, pt, &columns[i].name)?;
                        columns[i].ty = pt;
                        head_unknown[i] = false;
                    }
                    (false, true) => {
                        coerce_branch_column(&mut peer_rows, i, ht, &columns[i].name)?;
                    }
                    // Both untyped — nothing to resolve against yet.
                    (true, true) => {}
                }
            }
            // v7.37 D.26 — a UNION result column is nullable when ANY branch is
            // nullable (PG semantics). Previously the result kept only the head's
            // nullability, so `VALUES (1),(NULL)` (a UNION-ALL chain seeded by the
            // non-null `1`) wrongly reported the column NOT NULL, which let
            // `count(col)`'s NOT-NULL fast-path count the NULL row.
            for (i, pc) in peer_cols.iter().enumerate() {
                if pc.nullable {
                    columns[i].nullable = true;
                }
            }
            // v7.39 (round 410) — under MySQL, set-op dedup / matching folds
            // text by the session collation (CI + accent + PAD SPACE), like
            // GROUP BY. PG stays byte-exact.
            let mysql = self.backslash_escapes;
            match kind {
                UnionKind::All => rows.extend(peer_rows),
                UnionKind::Distinct => {
                    rows.extend(peer_rows);
                    rows = dedup_rows(rows, mysql);
                }
                // v7.37.17 (17.6 siblings) — PG set semantics.
                // v7.39 (round 591) — all four ask the same question of the
                // right side, and all four used to answer it by scanning it
                // once per left row. `PeerIndex` buckets it by the hash
                // DISTINCT already uses, so the answer is a lookup.
                // INTERSECT: distinct rows present on both sides.
                UnionKind::Intersect => {
                    let idx = PeerIndex::build(&peer_rows, mysql);
                    rows = dedup_rows(rows, mysql)
                        .into_iter()
                        .filter(|r| idx.contains(r))
                        .collect();
                }
                // INTERSECT ALL: multiset intersection — each row
                // keeps min(left count, right count) occurrences.
                UnionKind::IntersectAll => {
                    let mut idx = PeerIndex::build(&peer_rows, mysql);
                    let mut kept: Vec<Row<'static>> = Vec::new();
                    for r in rows {
                        if idx.take_one(&r) {
                            kept.push(r);
                        }
                    }
                    rows = kept;
                }
                // EXCEPT: distinct left rows absent from the right.
                UnionKind::Except => {
                    let idx = PeerIndex::build(&peer_rows, mysql);
                    rows = dedup_rows(rows, mysql)
                        .into_iter()
                        .filter(|r| !idx.contains(r))
                        .collect();
                }
                // EXCEPT ALL: multiset subtraction — each right
                // occurrence cancels one left occurrence.
                UnionKind::ExceptAll => {
                    let mut idx = PeerIndex::build(&peer_rows, mysql);
                    let mut kept: Vec<Row<'static>> = Vec::new();
                    for r in rows {
                        if !idx.take_one(&r) {
                            kept.push(r);
                        }
                    }
                    rows = kept;
                }
            }
        }
        // PG resolves a UNION / VALUES result column to one common type
        // and casts every branch to it (`SELECT '2020-01-01'::date UNION
        // ALL SELECT '2020-01-02'` → both DATE, not DATE + TEXT). SPG
        // built each branch independently, leaving mixed-type columns
        // that broke ORDER BY, comparisons, and value-based window
        // frames. Unify + coerce before the combined ORDER BY sees them.
        unify_union_columns(&mut columns, &mut rows);
        // ORDER BY at the top of a UNION applies to the combined result.
        // Eval against the projected schema (NOT the source table).
        if !stmt.order_by.is_empty() {
            // v7.39 (read01 round 54) — the combined-result ctx must carry the
            // catalog, and the projected columns must keep their enum identity
            // (`user_enum_type`), or `ORDER BY <enum col>` over a UNION sorts
            // by TEXT instead of member order — silently wrong rows, not an
            // error. (Same shape as the enum-order knife's GROUP BY fix.)
            let synth_ctx = EvalContext::new(&columns, None).with_catalog(self.active_catalog());
            // v7.37.17 (17.6 siblings) — positional keys (ORDER BY 1)
            // survive to here when the head projects a Wildcard (the
            // group-tail wrapper shape): map them onto the Nth
            // projected column so the combined sort works.
            let resolved_order: Vec<spg_sql::ast::OrderBy> = stmt
                .order_by
                .iter()
                .map(|o| {
                    let mut o = o.clone();
                    if let Expr::Literal(spg_sql::ast::Literal::Integer(n)) = &o.expr
                        && *n >= 1
                        && let Ok(idx) = usize::try_from(*n - 1)
                        && idx < columns.len()
                    {
                        o.expr = Expr::Column(spg_sql::ast::ColumnName {
                            qualifier: None,
                            name: columns[idx].name.clone(),
                        });
                    }
                    o
                })
                .collect();
            let descs: Vec<bool> = resolved_order.iter().map(|o| o.desc).collect();
            let mut tagged: Vec<(Vec<OrderKey>, Row)> = Vec::with_capacity(rows.len());
            for r in rows {
                let keys = build_order_keys(&resolved_order, &r, &synth_ctx)?;
                tagged.push((keys, r));
            }
            sort_by_keys(&mut tagged, &descs);
            rows = tagged.into_iter().map(|(_, r)| r).collect();
        }
        apply_offset_and_limit(&mut rows, stmt.offset_literal(), stmt.limit_literal());
        Ok(QueryResult::Rows { columns, rows })
    }

    fn exec_select_cancel_inner(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v7.38 P0 元机制 A — first observable point inside the
        // planner / executor. Tests use this to inject a delay or
        // a cancellation race before any row is produced. Release
        // build expands to `let _ = (...);` — zero cost.
        crate::injection_point!("planner_first_row_fetch", &stmt.from);
        // v7.39 (round 705) — WINDOW-clause definitions nothing referenced.
        // PG analyses every definition, referenced or not, so `SELECT i FROM
        // t WINDOW w AS (ORDER BY nosuch)` fails there and silently
        // succeeded here (the parser used to drop the unreferenced defs
        // whole). The check is the CREATE VIEW check's shape (round 700): a
        // LIMIT-0 run of the same FROM with the definitions' key
        // expressions as the projection — it cannot disagree with what a
        // referencing window would have done, because it resolves the same
        // names the same way. Zero cost for the ordinary statement: the
        // list is empty unless a WINDOW clause left unreferenced defs.
        if !stmt.window_check_exprs.is_empty() {
            let mut probe = stmt.clone();
            probe.items = stmt
                .window_check_exprs
                .iter()
                .map(|e| spg_sql::ast::SelectItem::Expr {
                    expr: e.clone(),
                    alias: None,
                })
                .collect();
            probe.window_check_exprs = Vec::new();
            probe.distinct = false;
            probe.distinct_on = Vec::new();
            probe.group_by = None;
            probe.group_by_all = false;
            probe.having = None;
            probe.unions = Vec::new();
            probe.order_by = Vec::new();
            probe.locking = None;
            probe.limit = Some(spg_sql::ast::LimitExpr::Literal(0));
            probe.offset = None;
            probe.limit_with_ties = false;
            self.exec_select_cancel_inner(&probe, cancel)?;
        }
        // v7.39 (read01 round 74) — lower `(f(args)).*`. Naming a record's fields
        // takes the catalog, so the parser leaves a marker and the rewrite lands
        // here: the call moves into a LATERAL FROM item and the item becomes one
        // reference per declared column. `SELECT 'p', (rows_of(2)).*` is
        // `SELECT 'p', __rec.id, __rec.v FROM rows_of(2) AS __rec` — reusing the
        // set-returning FROM machinery of rounds 65 and 69 rather than growing a
        // second one.
        if let Some(lowered) = self.lower_record_expansion(stmt)? {
            return self.exec_select_cancel_inner(&lowered, cancel);
        }
        // v7.17.0 Phase 1.2 — user-defined VIEW expansion. If the
        // FROM / JOIN graph references any catalogued view name,
        // re-parse the view body and prepend it as a synthetic
        // CTE. Recurses on views-in-views via the regular CTE
        // dispatch below. Fast-path: skip the walker entirely when
        // the catalog has no views (the typical OLTP load).
        if !self.active_catalog().views_all().is_empty() {
            if let Some(rewritten) = self.expand_views_in_select(stmt)? {
                return self.exec_select_cancel(&rewritten, cancel);
            }
        }
        // v7.37.6-B(sentori Epic 2 P0)— `SELECT … FROM <partition-parent>`
        // gets rewritten to a UNION-ALL over the children that overlap
        // the WHERE-derived key range. Uses the same CTE-injection
        // trick as VIEW expansion above so downstream resolution
        // doesn't need a partition-aware code path.
        if let Some(rewritten) = self.expand_partition_parents_in_select(stmt)? {
            return self.exec_select_cancel(&rewritten, cancel);
        }
        // v7.16.2 — information_schema / pg_catalog virtual
        // views (mailrs round-10 A.3). If the SELECT touches a
        // synthetic meta-table name (`__spg_info_*` /
        // `__spg_pg_*` — produced by the parser for
        // `information_schema.X` / `pg_catalog.X`), clone the
        // catalog, materialise the requested view as a real
        // temporary table, and re-execute against an enriched
        // engine. Same pattern as `exec_with_ctes` for CTEs.
        if !self.meta_views_materialised && select_references_meta_view(stmt) {
            return self.exec_select_with_meta_views(stmt, cancel);
        }
        // v6.10.2 — cold-tier time-travel short-circuit. When the
        // primary TableRef carries `AS OF SEGMENT '<id>'`, run a
        // dedicated cold-segment scan instead of the regular
        // hot+index path. The scope is intentionally narrow for
        // v6.10.2 — bare `SELECT * FROM <t> AS OF SEGMENT 'id'`,
        // optionally with a single-column-equality WHERE. JOINs /
        // aggregates / ORDER BY / subqueries on top of a time-
        // travelled scan are STABILITY § "Out of v6.10".
        if let Some(from) = &stmt.from
            && let Some(seg_id) = from.primary.as_of_segment
        {
            return self.exec_select_as_of_segment(stmt, from, seg_id);
        }
        // v6.2.0 / v6.5.0 — virtual-table short-circuits. Detected
        // pre-CTE because they don't read from the catalog and
        // shouldn't participate in regular FROM resolution.
        // v6.2.0 / v6.5.0 / v7.38 (read01 P3.NEW3) — virtual-table
        // short-circuits. A meta-view FROM materialises to a fixed row
        // set. For a bare `SELECT *` we return it directly; otherwise we
        // stage it as a temp table and run the normal pipeline, so
        // projection / WHERE / ORDER BY / aggregates work over these views
        // (they were `SELECT *`-only before). A real table shadowing the
        // name wins (checked first), which also stops the staged re-run
        // from recursing back into meta-view detection.
        if let Some(from) = &stmt.from
            && from.joins.is_empty()
            && self.active_catalog().get(&from.primary.name).is_none()
        {
            let lower = from.primary.name.to_ascii_lowercase();
            if let Some(result) = self.meta_view_result(&lower) {
                let bare = stmt.where_.is_none()
                    && stmt.group_by.is_none()
                    && stmt.having.is_none()
                    && stmt.unions.is_empty()
                    && stmt.order_by.is_empty()
                    && stmt.limit.is_none()
                    && stmt.offset.is_none()
                    && !stmt.distinct
                    && stmt.items.iter().all(|i| matches!(i, SelectItem::Wildcard));
                if bare {
                    return Ok(result);
                }
                if let QueryResult::Rows { columns, rows } = result {
                    let mut catalog = self.active_catalog().clone();
                    let cols = infer_column_types(&columns, &rows);
                    let schema = TableSchema::new(from.primary.name.clone(), cols);
                    catalog.create_table(schema).map_err(EngineError::Storage)?;
                    let t = catalog
                        .get_mut(&from.primary.name)
                        .expect("just-created meta-view table must exist");
                    for row in rows {
                        t.insert(row).map_err(EngineError::Storage)?;
                    }
                    let mut eng = Engine::restore(catalog);
                    if let Some(c) = self.clock {
                        eng = eng.with_clock(c);
                    }
                    if let Some(f) = self.salt_fn {
                        eng = eng.with_salt_fn(f);
                    }
                    // v7.39 (read01 pgstatfuncs.c) — carry the calling-
                    // connection identity so `WHERE pid = pg_backend_pid()`
                    // matches inside the staged meta-view run.
                    if let Some(f) = self.backend_pid_fn {
                        eng.set_backend_pid_fn(f);
                    }
                    return eng.exec_select_cancel(stmt, cancel);
                }
                return Ok(result);
            }
        }
        // v4.11: CTEs materialise into a temporary enriched catalog
        // *before* anything else — the body SELECT can then refer
        // to CTE names via the regular FROM-clause resolution.
        // Uncorrelated only: each CTE body runs once against the
        // current catalog, not against later CTEs' results (left-
        // to-right materialisation would relax this, but we keep
        // it simple for v4.11 MVP).
        if !stmt.ctes.is_empty() {
            return self.exec_with_ctes(stmt, cancel);
        }
        // v4.10: subqueries (uncorrelated) are resolved here, before
        // the executor sees the row loop. We clone the statement so
        // we can mutate without disturbing the caller's AST — most
        // queries pass through with no subquery nodes and the clone
        // is cheap; with subqueries the materialisation cost
        // dominates anyway.
        let mut stmt_owned;
        let stmt_ref: &SelectStatement = if expr_tree_has_subquery(stmt) {
            stmt_owned = stmt.clone();
            // v7.33 (mailrs 7.32.1) — sublink pull-up first: an
            // aggregate-wrapped correlated scalar subquery whose
            // correlation key is UNIQUE/PK becomes a LEFT JOIN, so the
            // executor streams one join instead of splicing a per-row
            // subplan. Runs before the per-row/batch resolver, which then
            // only sees the subqueries the pull-up left behind.
            self.pull_up_unique_correlated_agg_subqueries(&mut stmt_owned);
            // v7.37.4 (A — correlated LIMIT 1 ORDER BY DESC pull-up) —
            // the "per-key latest" scalar subquery shape (inbox / feed
            // / timeline applications) becomes a CTE + LEFT JOIN
            // against a GROUP BY pre-aggregation that reuses the v7.33
            // first_ordered argmax executor. Runs AFTER unique-key
            // pull-up (so the unique-key fast path still wins for
            // single-PK lookups) and BEFORE the EXISTS sublink rewrite.
            // Phase 1 (this commit) is skeleton only — no-op pass.
            self.pull_up_correlated_limit_one_subqueries(&mut stmt_owned);
            // v7.34.2 (mailrs prod NOT EXISTS) — plan-time `[NOT] EXISTS`
            // sublink pull-up to semi/anti-join, before the resolver gets
            // a chance to walk per-row.
            self.pull_up_exists_sublinks(&mut stmt_owned);
            // v7.37.4 — if the LIMIT 1 pullup added CTEs, route through
            // exec_with_ctes so they materialise once before the body
            // SELECT runs. exec_with_ctes strips ctes from the body
            // clone, then re-enters select.
            if !stmt_owned.ctes.is_empty() {
                return self.exec_with_ctes(&stmt_owned, cancel);
            }
            // v7.37.x (docker-fair INSUBQ attack) — short-circuit
            //   SELECT COUNT(*) FROM A WHERE A.pk IN (<uncorrelated subquery>)
            // BEFORE `resolve_select_subqueries` materialises the inner
            // result as `Vec<Expr::Literal>` (~150 µs for the 6 k-row
            // INSUBQ benchmark). Run the inner once, collect the result
            // values into a `HashSet<i64>` directly, then probe A.pk per
            // value and tally. Returns `Some` when the shape matches.
            if let Some(out) = self.try_count_star_pk_in_subquery_fast(&stmt_owned, cancel)? {
                return Ok(out);
            }
            self.resolve_select_subqueries(&mut stmt_owned, cancel)?;
            &stmt_owned
        } else {
            stmt
        };
        if stmt_ref.unions.is_empty() {
            return self.exec_bare_select_cancel(stmt_ref, cancel);
        }
        self.exec_union_chain(stmt_ref, stmt, cancel)
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_lines)] // huge match — splitting fragments the planner
    /// v7.11.7 — execute `SELECT … FROM unnest(expr) [AS] alias …`.
    /// Synthesises a single-column virtual table whose column type
    /// is TEXT and whose rows are the array elements. Routes
    /// through the regular projection / WHERE / ORDER BY / LIMIT
    /// machinery so set-returning UNNEST composes naturally with
    /// the rest of the SELECT surface.
    fn exec_select_unnest(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let expr = primary
            .unnest_expr
            .as_deref()
            .expect("caller guards unnest_expr.is_some()");
        // Multi-arg unnest(a, b, …) — parallel zip, NULL-padded.
        // N value columns instead of one; the shared builder does
        // the work and the tail below (WHERE / agg / projection)
        // runs against the wider schema.
        let multi: Option<(alloc::vec::Vec<DataType>, alloc::vec::Vec<Row<'static>>)> =
            match unnest_zip_args(expr) {
                Some(args) => Some(unnest_zip_rows(args)?),
                None => None,
            };
        // Evaluate the array expression once. Empty schema / empty
        // row — uncorrelated UNNEST cannot reference outer columns.
        // v7.39 (read01 round 49) — the ctx must carry the catalog: the enum
        // introspection family (enum_range / enum_first / enum_last) resolves
        // its labels from the argument's STATIC enum type against the
        // catalog's enum registry. Without it `unnest(enum_range(NULL::mood))`
        // fell through to the generic arm, got NULL, and expanded to zero rows
        // — while the bare `SELECT enum_range(NULL::mood)` (whose ctx does
        // carry the catalog) worked.
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None).with_catalog(self.active_catalog());
        let dummy_row = Row::new(alloc::vec::Vec::new());
        // v7.11.13 — unnest dispatches per array element type so
        // INT[] / BIGINT[] surface their PG types in projection.
        // v7.39 (round 758, F31-B8a) — the composite SRF names its own
        // columns (PG: lexeme | positions | weights); everything else
        // keeps the alias / "unnest" defaults below.
        let mut composite_names: Option<&[&str]> = None;
        let (dtypes, rows): (alloc::vec::Vec<DataType>, alloc::vec::Vec<Row<'static>>) =
            if let Some(m) = multi {
                m
            } else {
                // v7.39 (round 236) — flatten a multidimensional array into
                // its row-major elements (PG) before the 1-D-only match.
                let unnest_src = {
                    let v = eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)?;
                    crate::eval::values::flatten_2d(&v).unwrap_or(v)
                };
                let mut return_multi: Option<(
                    alloc::vec::Vec<DataType>,
                    alloc::vec::Vec<Row<'static>>,
                )> = None;
                let (elem_dtype, rows): (DataType, alloc::vec::Vec<Row<'static>>) = match unnest_src
                {
                    Value::Null => (DataType::Text, alloc::vec::Vec::new()),
                    Value::TextArray(items) => {
                        let rows = items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(s) => Value::text(s),
                                    None => Value::Null,
                                }])
                            })
                            .collect();
                        (DataType::Text, rows)
                    }
                    Value::IntArray(items) => {
                        let rows = items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::Int(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect();
                        (DataType::Int, rows)
                    }
                    Value::BigIntArray(items) => {
                        let rows = items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::BigInt(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect();
                        (DataType::BigInt, rows)
                    }
                    Value::Multirange { kind, ranges } => {
                        let rows = ranges
                            .iter()
                            .map(|sp| {
                                Row::new(alloc::vec![Value::Range {
                                    kind,
                                    lower: sp.lower.clone(),
                                    upper: sp.upper.clone(),
                                    lower_inc: sp.lower_inc,
                                    upper_inc: sp.upper_inc,
                                    empty: false,
                                }])
                            })
                            .collect();
                        (DataType::Range(kind), rows)
                    }
                    // v7.39 (round 758, F31-B8a) — unnest(tsvector):
                    // one row per lexeme, PG18-measured columns
                    // lexeme | positions | weights (`a | {1,3} |
                    // {D,D}`); a position-less lexeme (a stripped
                    // vector) reads NULL in both array columns.
                    Value::TsVector(lexemes) => {
                        composite_names = Some(&["lexeme", "positions", "weights"]);
                        let rows = lexemes
                            .iter()
                            .map(|l| {
                                let (pos, wts) = if l.positions.is_empty() {
                                    (Value::Null, Value::Null)
                                } else {
                                    let letter = match l.weight {
                                        3 => "A",
                                        2 => "B",
                                        1 => "C",
                                        _ => "D",
                                    };
                                    (
                                        Value::SmallIntArray(
                                            l.positions
                                                .iter()
                                                .map(|p| {
                                                    Some(i16::try_from(*p).unwrap_or(i16::MAX))
                                                })
                                                .collect(),
                                        ),
                                        Value::TextArray(
                                            l.positions
                                                .iter()
                                                .map(|_| Some(letter.into()))
                                                .collect(),
                                        ),
                                    )
                                };
                                Row::new(alloc::vec![Value::text(l.word.clone()), pos, wts])
                            })
                            .collect();
                        return_multi = Some((
                            alloc::vec![
                                DataType::Text,
                                DataType::SmallIntArray,
                                DataType::TextArray
                            ],
                            rows,
                        ));
                        (DataType::Text, alloc::vec::Vec::new())
                    }
                    other => {
                        // v7.39 (round 622, S05a) — see table_access.rs:
                        // the same sentence, and it is a type mismatch.
                        return Err(EngineError::Eval(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "unnest() expects an array argument, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        }));
                    }
                };
                if let Some(m) = return_multi {
                    m
                } else {
                    (alloc::vec![elem_dtype], rows)
                }
            };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "unnest".to_string());
        // v7.13.2 — mailrs round-6 S5. Honour PG-standard
        // `UNNEST(arr) AS p(col_name)` column-list aliasing:
        // entries map positionally over the value columns. Without
        // the column list, a single column falls back to the table
        // alias (pre-v7.13.2 behaviour); multi-arg columns default
        // to PG's `unnest`.
        let n_vals = dtypes.len();
        let mut schema_cols: alloc::vec::Vec<ColumnSchema> = dtypes
            .iter()
            .enumerate()
            .map(|(i, dt)| {
                let name = primary
                    .unnest_column_aliases
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| {
                        if let Some(names) = composite_names {
                            names
                                .get(i)
                                .map_or_else(|| "unnest".to_string(), |n| (*n).to_string())
                        } else if n_vals == 1 {
                            alias.clone()
                        } else {
                            "unnest".to_string()
                        }
                    });
                ColumnSchema::new(name, *dt, true)
            })
            .collect();
        // v7.39 (read01 round 78) — the item's row type IS this scalar when the
        // parser desugared a base-type-returning function here (see
        // TableRef::scalar_fn_item); the marker rides the column so it survives
        // every EvalContext an inner stage rebuilds.
        if primary.scalar_fn_item && schema_cols.len() == 1 {
            schema_cols[0].scalar_row_source = true;
        }
        // WITH ORDINALITY — trailing BIGINT counting rows from 1
        // in element order. The alias entry after the value
        // columns renames it (PG default: `ordinality`).
        let rows = if primary.with_ordinality {
            let ord_name = primary
                .unnest_column_aliases
                .get(n_vals)
                .cloned()
                .unwrap_or_else(|| "ordinality".to_string());
            schema_cols.push(ColumnSchema::new(ord_name, DataType::BigInt, false));
            rows.into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let mut vals = row.values.clone();
                    vals.push(Value::BigInt(i as i64 + 1));
                    Row::new(vals)
                })
                .collect()
        } else {
            rows
        };
        // v7.39 (read01 round 54) — `ev_ctx` threads the catalog; a bare
        // `EvalContext::new` drops it and every catalog-dependent cast
        // (regclass / enum / composite / domain) silently degrades.
        let scan_ctx = self.ev_ctx(&schema_cols, Some(&alias));
        // Apply WHERE.
        let filtered: alloc::vec::Vec<Row<'static>> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = eval::eval_expr(w, &row, &scan_ctx).map_err(EngineError::Eval)?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // v7.17.0 Phase 3.P0-48 — aggregate dispatch over the
        // unnest source. Same routing the relational scan path
        // already takes — without it `SELECT COUNT(*) FROM
        // unnest(ARRAY[…])` either errored at projection time or
        // returned the wrong shape.
        if aggregate::uses_aggregate(stmt) {
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            // v7.39 (round 656) — hand the rows over as they are rather than
            // collecting a second vector of `RowRef` wrappers. Note this is
            // a set-returning-function path, NOT the relational scan: the
            // measured O(rows) cost lived in `run_single_table_aggregate`,
            // and converting these four first was a miss that cost a full
            // round — every test stayed green and the number did not move.
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Owned(&filtered),
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection =
            build_projection(&stmt.items, &schema_cols, &alias, self.backslash_escapes)?;
        let mut projected_rows: alloc::vec::Vec<Row<'static>> =
            alloc::vec::Vec::with_capacity(filtered.len());
        // v7.19 P5 — Set-Returning-Function in projection
        // position (PG `SELECT unnest(arr) FROM t` shape). When a
        // SELECT item evaluates to a top-level unnest(arr) call,
        // expand it: for each input row, evaluate the array, emit
        // one output row per element, broadcasting non-SRF
        // projections from the same input row. Multi-SRF + LCM
        // padding stays a documented carve-out; mailrs uses
        // single-SRF for redirect_uris.
        // v7.39 (read01 round 67) — EVERY set-returning item expands, in lockstep
        // (see `expand_srf_row`); a user `RETURNS SETOF` function counts too.
        let srf_idxs = self.srf_target_idxs(&projection);
        // v7.39 (round 621) — which input row each output row came from. An
        // SRF turns one input row into many, and the ORDER BY below used to
        // index the EXPANDED rows by the INPUT row's position: the result was
        // silently truncated to the input row count and left unsorted, so
        // `SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1`
        // answered three of its six rows, in no order. Without the ORDER BY
        // the same query was already right.
        let mut src_of_row: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        if !srf_idxs.is_empty() {
            let (rows, src) =
                expand_projection_srfs(self, &projection, &srf_idxs, &filtered, &scan_ctx)?;
            projected_rows = rows;
            src_of_row = src;
        } else {
            // v7.24 (round-16 B) — select-list subqueries resolve
            // per row (correlated-aware; plain exprs take the fast
            // path inside).
            let mut proj_memo = memoize::MemoizeCache::default();
            for row in &filtered {
                let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                for p in &projection {
                    vals.push(self.eval_expr_with_correlated(
                        &p.expr,
                        row,
                        &scan_ctx,
                        cancel,
                        Some(&mut proj_memo),
                    )?);
                }
                projected_rows.push(Row::new(vals));
            }
        }
        // ORDER BY / LIMIT — apply on the projected rows (cheap;
        // unnest result sets are small by design).
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            // v7.39 (read01 round 54) — keep the column's enum identity through
            // the projection (it lives outside the DataType lattice), or a
            // derived table / UNION / windowed result forgets it and any outer
            // `ORDER BY <enum col>` silently sorts by the label's TEXT.
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        // Re-evaluate ORDER BY against the source schema (pre-projection
        // so col refs by name still resolve through `scan_ctx`).
        // v7.39 (read01 round 80) — a positional key means the Nth OUTPUT
        // column. Evaluated as an expression it is just the constant N: the same
        // key for every row, so the sort ran and changed nothing.
        let order_by = resolve_positional_order_by(&stmt.order_by, &projection);
        if !order_by.is_empty() {
            // v7.39 (round 621) — one entry per OUTPUT row, not per input row.
            // A key that names a select-list item reads it out of the expanded
            // row (PG sorts AFTER the expansion); one that names a source
            // column the query does not project is evaluated on the input row
            // it came from, which is what `srf_order_output_cols` decides.
            let out_cols = if srf_idxs.is_empty() {
                alloc::vec![None; order_by.len()]
            } else {
                srf_order_output_cols(&order_by, &projection)
            };
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value<'static>>)> = projected_rows
                .iter()
                .enumerate()
                .map(|(k, out)| -> Result<_, EngineError> {
                    let src = src_of_row.get(k).copied().unwrap_or(k);
                    let keys: Result<Vec<Value<'static>>, EngineError> = order_by
                        .iter()
                        .zip(out_cols.iter())
                        .map(|(ob, oc)| srf_order_key(ob, *oc, out, &filtered[src], &scan_ctx))
                        .collect();
                    Ok((k, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &order_by[idx];
                    let cmp = order_by_value_cmp_in(
                        o.desc,
                        o.nulls_first,
                        ka,
                        kb,
                        scan_ctx.mysql_dialect && !crate::eval::is_binary_coerced(&o.expr),
                    );
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        // v7.38 (read01) — DISTINCT over a synthetic source was dropped here.
        if stmt.distinct {
            projected_rows = dedup_rows(projected_rows, scan_ctx.mysql_dialect);
        }
        // LIMIT / OFFSET — apply at the tail.
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    /// v7.17.0 Phase 3.10 — `FROM generate_series(start, stop [,
    /// step])` set-returning source. Mirrors `exec_select_unnest`'s
    /// shape: evaluate the arg list once against an empty row,
    /// materialise the row stream by stepping start → stop, then
    /// route through the standard WHERE / projection / ORDER BY /
    /// LIMIT pipeline. Two arg-type combos in v7.17:
    ///   * integer / integer [/ integer] — SmallInt, Int, BigInt
    ///     (widened to BigInt internally; step defaults to 1)
    ///   * timestamp / timestamp / interval — date-range
    ///     iteration (mailrs's daily-report pattern)
    fn exec_select_generate_series(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let args = primary
            .generate_series_args
            .as_ref()
            .expect("caller guards generate_series_args.is_some()");
        let (elem_dtype, rows) = generate_series_rows(args, &cancel)?;
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "generate_series".to_string());
        // `AS t(n)` — the first column-alias entry renames the
        // series column (PG semantics); bare alias keeps the
        // pre-existing behaviour of naming the column after it.
        let col_name = primary
            .unnest_column_aliases
            .first()
            .cloned()
            .unwrap_or_else(|| alias.clone());
        let col_schema = ColumnSchema::new(col_name, elem_dtype, true);
        let mut schema_cols = alloc::vec![col_schema.clone()];
        // WITH ORDINALITY — trailing BIGINT counting rows from 1;
        // the second column-alias entry renames it.
        let rows = if primary.with_ordinality {
            let ord_name = primary
                .unnest_column_aliases
                .get(1)
                .cloned()
                .unwrap_or_else(|| "ordinality".to_string());
            schema_cols.push(ColumnSchema::new(ord_name, DataType::BigInt, false));
            rows.into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let mut vals = row.values.clone();
                    vals.push(Value::BigInt(i as i64 + 1));
                    Row::new(vals)
                })
                .collect()
        } else {
            rows
        };
        // v7.39 (read01 round 54) — `ev_ctx` threads the catalog; a bare
        // `EvalContext::new` drops it and every catalog-dependent cast
        // (regclass / enum / composite / domain) silently degrades.
        let scan_ctx = self.ev_ctx(&schema_cols, Some(&alias));
        // WHERE.
        let filtered: alloc::vec::Vec<Row<'static>> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = eval::eval_expr(w, &row, &scan_ctx).map_err(EngineError::Eval)?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // v7.17.0 Phase 3.P0-48 — aggregate dispatch for set-
        // returning sources. When the SELECT projection contains
        // aggregate functions (COUNT/SUM/MIN/MAX/AVG/string_agg/
        // …) we route the filtered row stream through the same
        // aggregate executor the relational scan path uses, so
        // `SELECT COUNT(*) FROM generate_series(1, 100)` returns
        // a single 100 row instead of erroring at projection
        // time. GROUP BY / HAVING / ORDER BY over the aggregate
        // output all ride through `aggregate::run`.
        if aggregate::uses_aggregate(stmt) {
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            // v7.39 (round 656) — hand the rows over as they are rather than
            // collecting a second vector of `RowRef` wrappers. Note this is
            // a set-returning-function path, NOT the relational scan: the
            // measured O(rows) cost lived in `run_single_table_aggregate`,
            // and converting these four first was a miss that cost a full
            // round — every test stayed green and the number did not move.
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Owned(&filtered),
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection =
            build_projection(&stmt.items, &schema_cols, &alias, self.backslash_escapes)?;
        // v7.39 (round 621) — and here, for the same reason.
        let srf_idxs = self.srf_target_idxs(&projection);
        let mut src_of_row: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        let mut projected_rows: alloc::vec::Vec<Row<'static>> =
            alloc::vec::Vec::with_capacity(filtered.len());
        let mut proj_memo = memoize::MemoizeCache::default();
        if !srf_idxs.is_empty() {
            let (rows, src) =
                expand_projection_srfs(self, &projection, &srf_idxs, &filtered, &scan_ctx)?;
            projected_rows = rows;
            src_of_row = src;
        } else {
            for row in &filtered {
                let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                for p in &projection {
                    // v7.24 (round-16 B) — correlated-aware.
                    vals.push(self.eval_expr_with_correlated(
                        &p.expr,
                        row,
                        &scan_ctx,
                        cancel,
                        Some(&mut proj_memo),
                    )?);
                }
                projected_rows.push(Row::new(vals));
            }
        }
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            // v7.39 (read01 round 54) — keep the column's enum identity through
            // the projection (it lives outside the DataType lattice), or a
            // derived table / UNION / windowed result forgets it and any outer
            // `ORDER BY <enum col>` silently sorts by the label's TEXT.
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        // ORDER BY against the source schema.
        // v7.39 (round 621) — one entry per OUTPUT row (a target-list SRF makes
        // more of them than there were inputs), and a positional key means the
        // Nth OUTPUT column, which is what `resolve_positional_order_by` does
        // and what the other two synthetic-source tails already did.
        let order_by = resolve_positional_order_by(&stmt.order_by, &projection);
        if !order_by.is_empty() {
            let out_cols = if srf_idxs.is_empty() {
                alloc::vec![None; order_by.len()]
            } else {
                srf_order_output_cols(&order_by, &projection)
            };
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value<'static>>)> = projected_rows
                .iter()
                .enumerate()
                .map(|(k, out)| -> Result<_, EngineError> {
                    let r = &filtered[src_of_row.get(k).copied().unwrap_or(k)];
                    let keys: Result<Vec<Value<'static>>, EngineError> = order_by
                        .iter()
                        .zip(out_cols.iter())
                        .map(|(ob, oc)| srf_order_key(ob, *oc, out, r, &scan_ctx))
                        .collect();
                    Ok((k, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &stmt.order_by[idx];
                    let cmp = order_by_value_cmp_in(
                        o.desc,
                        o.nulls_first,
                        ka,
                        kb,
                        scan_ctx.mysql_dialect && !crate::eval::is_binary_coerced(&o.expr),
                    );
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        // v7.38 (read01) — DISTINCT over a synthetic source was dropped here.
        if stmt.distinct {
            projected_rows = dedup_rows(projected_rows, scan_ctx.mysql_dialect);
        }
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    /// The FROM shapes that are not an ordinary table scan — joins, the
    /// set-returning sources, JSON_TABLE, a derived table, and the rest.
    ///
    /// `#[inline(never)]` and out of `exec_bare_select_cancel` for the
    /// reason round 848 established in the parser: a debug build gives
    /// EVERY branch's locals a slot in the frame, whichever branch runs.
    /// `exec_bare_select_cancel` measured 64,784 bytes and a nested query
    /// stacks several of them; a plain scan reaches none of these
    /// branches. Moving them out took the frame to 52,336.
    ///
    /// `Ok(None)` means "not one of these shapes, carry on".
    #[inline(never)]
    fn try_from_shape_paths(
        &self,
        stmt: &SelectStatement,
        from: &spg_sql::ast::FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        if !from.joins.is_empty() {
            // v7.37.x (docker-fair LEFTJOIN 71 % attack) — LEFT JOIN
            // elimination: when a LEFT JOIN's right side is referenced
            // ONLY in the ON equality and the right-side join key is
            // UNIQUE/PK, the join preserves outer cardinality exactly
            // and contributes no values used downstream. Drop the
            // entire join. PG does this on the
            // `SELECT COUNT(*) FROM A LEFT JOIN B ON B.pk = A.fk` shape
            // — A's row count is what survives, B never has to be
            // touched.
            if let Some(eliminated) = self.try_eliminate_redundant_left_joins(stmt) {
                return self.exec_bare_select_cancel(&eliminated, cancel).map(Some);
            }
            // v7.38 P0 元机制 D — `SPG_TEST_DISABLE_JOINFOLD=1` skips
            // the v7.32 joinfold rewrite that turns inner JOINs into a
            // single-table scan when the catalogue can prove key-only
            // dependency. Tests use this to assert "without joinfold,
            // the join still executes correctly" (joinfold is a
            // semantically-equivalent rewrite, not a correctness fix).
            if !self.env_cfg().disable_joinfold {
                if let Some(folded) = self.try_fold_inner_joins(stmt, cancel)? {
                    return self.exec_bare_select_cancel(&folded, cancel).map(Some);
                }
            }
            return self.exec_joined_select(stmt, from, cancel).map(Some);
        }
        // v7.11.7 — `FROM unnest(<expr>) [AS] <alias>`. Synthesise a
        // single-column table at SELECT entry by evaluating the
        // expression once against the empty row (UNNEST is
        // uncorrelated in v7.11; correlated / LATERAL unnest is a
        // v7.12 carve-out). Build a virtual `Table` in a heap-only
        // catalog, then route to the regular scan path.
        if from.primary.unnest_expr.is_some() {
            return self
                .exec_select_unnest(stmt, &from.primary, cancel)
                .map(Some);
        }
        // v7.37.43-T4.5 — `FROM jsonb_each_text(<expr>)` set-
        // returning function. Same dispatch shape as unnest but
        // emits a two-column (key TEXT, value TEXT) row stream.
        if from.primary.jsonb_each_text_arg.is_some() {
            return self
                .exec_select_jsonb_each_text(stmt, &from.primary, cancel)
                .map(Some);
        }
        // v7.39 (read01 partitionfuncs.c) — FROM-position table functions
        // (pg_partition_tree / pg_partition_ancestors) dispatched by name.
        // v7.39 (read01 round 74) — `ROWS FROM (f(a), g(b))` whose entries have no
        // array form. Each function runs; the results zip in LOCKSTEP with the
        // shorter padded to NULL — the SAME rule the target-list SRFs follow
        // (round 67), which is why `srf_values` is what evaluates each entry.
        if from.primary.rows_from.is_some() {
            let (rows, mut schema_cols) = self.rows_from_rows(&from.primary)?;
            for (i, new_name) in from.primary.unnest_column_aliases.iter().enumerate() {
                if let Some(col) = schema_cols.get_mut(i) {
                    col.name = new_name.clone();
                }
            }
            let alias = from
                .primary
                .alias
                .clone()
                .unwrap_or_else(|| from.primary.name.clone());
            return self
                .exec_select_over_rows(stmt, rows, schema_cols, &alias, cancel)
                .map(Some);
        }
        // v7.39 (round 205, JSON_TABLE) — `FROM JSON_TABLE(doc, '$p'
        // COLUMNS (...))`. Materialise the row stream + schema by
        // walking the row path, then run the regular pipeline over it.
        if let Some(jt) = &from.primary.json_table {
            let (rows, schema_cols) = self.json_table_rows(jt, None)?;
            let alias = from
                .primary
                .alias
                .clone()
                .unwrap_or_else(|| from.primary.name.clone());
            return self
                .exec_select_over_rows(stmt, rows, schema_cols, &alias, cancel)
                .map(Some);
        }
        if from.primary.table_fn_call.is_some() {
            let (rows, mut schema_cols) = self.table_fn_rows(&from.primary)?;
            // v7.39 (read01 round 68) — WITH ORDINALITY appends a BIGINT counter
            // (from 1, in output order) AFTER the function's own columns. The
            // alias list names it like any other, which is why it is appended
            // BEFORE the renaming pass below.
            let rows = if from.primary.with_ordinality {
                schema_cols.push(ColumnSchema::new(
                    "ordinality".to_string(),
                    DataType::BigInt,
                    false,
                ));
                rows.into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let mut vals = r.values;
                        vals.push(Value::BigInt(i as i64 + 1));
                        Row::new(vals)
                    })
                    .collect()
            } else {
                rows
            };
            for (i, new_name) in from.primary.unnest_column_aliases.iter().enumerate() {
                if let Some(col) = schema_cols.get_mut(i) {
                    col.name = new_name.clone();
                }
            }
            let alias = from
                .primary
                .alias
                .clone()
                .unwrap_or_else(|| from.primary.name.clone());
            return self
                .exec_select_over_rows(stmt, rows, schema_cols, &alias, cancel)
                .map(Some);
        }
        // v7.37.17 (17.6 siblings) — plain derived table in primary
        // position: `FROM ( SELECT … ) alias` (no joins). The inner
        // SELECT materialises once (it is uncorrelated by
        // construction), then the outer projection / WHERE /
        // aggregate / ORDER BY pipeline runs over the synthetic
        // table. Joined derived tables keep riding the LATERAL
        // machinery in join.rs.
        if from.joins.is_empty() && from.primary.lateral_subquery.is_some() {
            // v7.39 (round 727) — flatten first. A simple derived table
            // (bare-column projection over one stored table, nothing that
            // changes cardinality or order) used to force the inner
            // SELECT through the SERIAL row-at-a-time projection pipeline
            // just to materialise a synthetic table the outer query then
            // re-scans: `count(*) FROM (SELECT id v FROM d WHERE …) q`
            // measured 18.6 ms against PG's 5 — and bare count over the
            // same filter WITHOUT the wrapper is 2 ms here, because it
            // rides the fused parallel lane. Rewriting to the unwrapped
            // form is PG's subquery pull-up; the whole tree gets the
            // fast lanes back.
            if let Some(flat) = try_flatten_derived(stmt, &from.primary) {
                return self.exec_select_cancel(&flat, cancel).map(Some);
            }
            // v7.39 (round 742) — `SELECT count(*) FROM (SELECT … ORDER
            // BY … OFFSET k) q` is `greatest(count_of_inner - k, 0)`:
            // ORDER BY never changes the row count, and OFFSET drops
            // exactly k. The materialising path sorted 500k rows to
            // count 10k (57 ms); PG runs its parallel sort anyway
            // (28 ms). The rewrite skips the sort entirely on both
            // counts — a plan PG itself does not have.
            if let Some(rewritten) = try_count_over_offset(stmt, &from.primary) {
                return self.exec_select_cancel(&rewritten, cancel).map(Some);
            }
            // v7.39 (round 743) — `count(*) OVER a derived whose only
            // item is unnest(ARRAY[k elements])` is `k * count(WHERE)`:
            // a constant-length array unnests to exactly k rows per
            // input row, NULL elements included. PG expands the set to
            // count it (6.6 ms on the panel cell); the identity doesn't.
            if let Some(rewritten) = try_count_over_const_unnest(stmt, &from.primary) {
                return self.exec_select_cancel(&rewritten, cancel).map(Some);
            }
            return self
                .exec_select_derived(stmt, &from.primary, cancel)
                .map(Some);
        }
        // v7.17.0 Phase 3.10 — `FROM generate_series(start, stop
        // [, step])` set-returning source. Dispatch mirrors UNNEST:
        // materialise the row stream from a single eval pass, then
        // run the regular projection / WHERE / ORDER BY / LIMIT
        // pipeline over the synthetic single-column table.
        if from.primary.generate_series_args.is_some() {
            return self
                .exec_select_generate_series(stmt, &from.primary, cancel)
                .map(Some);
        }
        Ok(None)
    }

    /// Pick an index seek for this WHERE, if any of the four apply:
    /// BTree equality, GIN `@@`, trigram LIKE, or JSONB `@>`.
    ///
    /// `#[inline(never)]` and out of `exec_bare_select_cancel` for the
    /// frame reason on `try_from_shape_paths`: in a debug build a
    /// closure's locals belong to the enclosing frame, and this one is
    /// four seek attempts wide on a function that nests.
    #[inline(never)]
    fn pick_indexed_rows<'r>(
        &'r self,
        stmt: &SelectStatement,
        table: &'r spg_storage::Table,
        schema_cols: &[spg_storage::ColumnSchema],
        alias: &str,
        ctx: &crate::eval::EvalContext<'_>,
        seek_snapshot: &crate::Snapshot,
    ) -> Option<Vec<Cow<'r, Row<'static>>>> {
        stmt.where_.as_ref().and_then(|w| {
            // BTree / col=literal seek first — covers the v7.11.3 multi-
            // column AND case and the leading-column equality lookup.
            try_index_seek(
                w,
                schema_cols,
                self.active_catalog(),
                table,
                alias,
                seek_snapshot,
            )
            .or_else(|| {
                // v7.12.3 — GIN-accelerated `WHERE col @@
                // tsquery` when the column has a `USING gin`
                // index. Returns an over-approximate candidate
                // set; the WHERE re-eval loop below verifies
                // the full `@@` predicate per row.
                try_gin_seek(
                    w,
                    schema_cols,
                    self.active_catalog(),
                    table,
                    alias,
                    ctx,
                    seek_snapshot,
                )
            })
            .or_else(|| {
                // v7.15.0 — trigram-GIN-accelerated
                // `WHERE col LIKE / ILIKE '<pat>'` when the
                // column has a `gin_trgm_ops` GIN index.
                // Over-approximate candidate set; the WHERE
                // re-eval verifies the LIKE per row.
                try_trgm_seek(w, schema_cols, table, alias, seek_snapshot)
            })
            .or_else(|| {
                // v7.37.8(sentori Epic 5 P2)— real JSONB-GIN
                // accelerated `WHERE col @> <jsonb_literal>`
                // when the column has a `USING gin` index. The
                // posting-list intersection returns an over-
                // approximate candidate set; the WHERE re-eval
                // verifies the full `@>` predicate per row.
                try_gin_jsonb_seek(w, schema_cols, table, alias, seek_snapshot)
            })
        })
    }

    /// Index-seek fast paths: NSW kNN, the primary-key top-N walk, and
    /// the two `count(*)` short-circuits. Out-of-line for the frame
    /// reason on `try_from_shape_paths` — an ordinary scan reaches none
    /// of them, and in a debug build their locals sit in the frame
    /// regardless.
    #[inline(never)]
    fn try_seek_fast_paths(
        &self,
        stmt: &SelectStatement,
        table: &spg_storage::Table,
        schema_cols: &[spg_storage::ColumnSchema],
        alias: &str,
        seek_snapshot: &crate::Snapshot,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        if let Some(nsw_rows) = try_nsw_knn(stmt, table, schema_cols, alias, seek_snapshot) {
            // NSW kNN dispatches against the hot-tier vector index only
            // (vector cells aren't promoted to cold segments), so wrap
            // the returned row indices as `Cow::Borrowed` for the
            // unified `materialise_in_order` shape.
            let ordered: Vec<Cow<'_, Row<'static>>> = nsw_rows
                .into_iter()
                .filter_map(|i| table.rows().get(i).map(Cow::Borrowed))
                .collect();
            return materialise_in_order(
                stmt,
                schema_cols,
                alias,
                &ordered,
                self.backslash_escapes,
            )
            .map(Some);
        }

        // v7.34.5 — ORDER BY <indexed col> [DESC|ASC] LIMIT N drives
        // the scan via the BTree iterator in the requested direction
        // and stops after `OFFSET + LIMIT` candidates pass WHERE. The
        // 80 ms `mailrs_prod_plain_limit` baseline at 250 k rows is
        // the load-bearing consumer; this skips the materialise-every-
        // row + partial-sort tail entirely. Walker output is already
        // in ORDER BY order so `materialise_in_order` (no extra sort)
        // is the natural sink.
        if let Some(walked) = try_pk_walk_top_n(
            stmt,
            self.active_catalog(),
            table,
            schema_cols,
            alias,
            self,
            cancel,
        ) {
            return materialise_in_order(stmt, schema_cols, alias, &walked, self.backslash_escapes)
                .map(Some);
        }

        // Index seek: if WHERE is `col = literal` (or commuted) and the
        // referenced column has an index, dispatch each locator through
        // the catalog (hot tier → borrow, cold tier → page-read +
        // decode) and iterate just those rows. Otherwise fall back to a
        // v7.37.x (docker-fair INSUBQ attack) — short-circuit COUNT(*)
        // FROM A WHERE A.pk IN (large literal list). The post-subquery-
        // replacement shape of INSUBQ. Runs BEFORE `indexed_rows` so
        // we don't pay the row materialisation cost twice. Returns
        // a bare `Rows{count}` if the shape matches.
        if aggregate::uses_aggregate(stmt)
            && let Some(out) = self.try_count_star_pk_in_list_fast(stmt, table, schema_cols, alias)
        {
            return Ok(Some(out));
        }
        // v7.38 (perf) — `count(*) WHERE <indexed BETWEEN>`: count the in-range
        // locators directly, skipping row materialisation + WHERE re-eval.
        if aggregate::uses_aggregate(stmt)
            && let Some(out) = self.try_count_star_indexed_range_fast(
                stmt,
                table,
                schema_cols,
                alias,
                seek_snapshot,
            )
        {
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// The two rewrites that must happen before the FROM clause is even
    /// looked at: a meta-view reference needs the catalog views
    /// materialised, and a windowed projection belongs to the window
    /// executor. Out-of-line for the frame reason on
    /// `try_from_shape_paths`.
    #[inline(never)]
    fn try_pre_from_paths(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        if !self.meta_views_materialised && select_references_meta_view(stmt) {
            return self.exec_select_with_meta_views(stmt, cancel).map(Some);
        }
        // v4.12: window-function path. When the projection contains
        // any `name(args) OVER (...)` we route to the dedicated
        // executor — partition + sort + per-row window value before
        // the regular projection.
        if select_has_window(stmt) {
            // v7.37 D.23 — window functions run AFTER GROUP BY aggregation.
            // `SELECT g, sum(v), rank() OVER (ORDER BY sum(v)) FROM t GROUP BY g`
            // needs the aggregation done first, then windows over the grouped
            // rows. Rewrite to an aggregate derived subquery + outer window query
            // (which the window-over-derived path, D.13, executes). Only fires on
            // the currently-erroring agg+window+GROUP BY shape, so it can't
            // regress working window-only or aggregate-only queries.
            if let Some(rewritten) = rewrite_agg_before_window(stmt) {
                return self.exec_select_cancel(&rewritten, cancel).map(Some);
            }
            return self.exec_select_with_window(stmt, cancel).map(Some);
        }
        Ok(None)
    }

    /// A projection naming `ctid` or another system column: the schema
    /// has to be widened with them before the scan. Out-of-line for the
    /// frame reason on `try_from_shape_paths`.
    #[inline(never)]
    fn try_ctid_projection(
        &self,
        stmt: &SelectStatement,
        primary: &spg_sql::ast::TableRef,
        table: &spg_storage::Table,
        schema_cols: &[spg_storage::ColumnSchema],
        alias: &str,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        if references_ctid(stmt) {
            let snapshot = self.current_snapshot();
            let mut ext_cols = schema_cols.to_vec();
            for name in SYSTEM_COLUMNS {
                ext_cols.push(ColumnSchema::new(name.to_string(), DataType::Text, false));
            }
            let table_oid =
                crate::system_catalog::relation_oid(self.active_catalog(), &primary.name)
                    .unwrap_or(0);
            let headers = table.headers();
            let rows: Vec<Row<'static>> = table
                .scan_visible(&snapshot)
                .map(|(i, r)| {
                    let mut vals = r.values.clone();
                    // One block, offsets from 1, as PG numbers them.
                    vals.push(Value::Tid(0, i as u32 + 1));
                    let h = headers.get(i);
                    vals.push(Value::Xid(h.map_or(0, |h| h.xmin as u32)));
                    vals.push(Value::Xid(h.map_or(0, |h| h.xmax as u32)));
                    // SPG keeps no per-statement command ids; PG shows 0 for
                    // every row a reader can see, which is every row here.
                    vals.push(Value::Cid(0));
                    vals.push(Value::Cid(0));
                    vals.push(Value::BigInt(table_oid));
                    Row::new(vals)
                })
                .collect();
            return self
                .exec_select_over_rows(stmt, rows, ext_cols, alias, cancel)
                .map(Some);
        }
        Ok(None)
    }

    /// A sequence read as a one-row relation (`SELECT last_value FROM
    /// seq`), which PG allows and psql's \\d relies on. Out-of-line for
    /// the frame reason on `try_from_shape_paths`.
    #[inline(never)]
    fn try_sequence_relation(
        &self,
        stmt: &SelectStatement,
        primary: &spg_sql::ast::TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        if self.active_catalog().get(&primary.name).is_none()
            && let Some(seq) = self.active_catalog().sequence(&primary.name)
        {
            let rows = alloc::vec![Row::new(alloc::vec![
                Value::BigInt(seq.last_value),
                Value::BigInt(0),
                Value::Bool(seq.is_called),
            ])];
            let schema_cols = alloc::vec![
                ColumnSchema::new("last_value", DataType::BigInt, false),
                ColumnSchema::new("log_cnt", DataType::BigInt, false),
                ColumnSchema::new("is_called", DataType::Bool, false),
            ];
            let alias = primary
                .alias
                .clone()
                .unwrap_or_else(|| primary.name.clone());
            return self
                .exec_select_over_rows(stmt, rows, schema_cols, &alias, cancel)
                .map(Some);
        }
        Ok(None)
    }

    pub(crate) fn exec_bare_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.17.0 Phase 3.P0-49 — `FETCH FIRST N ROWS WITH TIES`
        // is meaningless without an ORDER BY; PG raises a hard
        // error and SPG mirrors the surface so the same DDL/app
        // path behaves identically on cutover.
        check_with_ties_requires_order_by(stmt)?;
        // v7.39 (round 229) — WHERE / HAVING run before the window pass, so
        // PG rejects window calls there outright. Checked here rather than
        // on the window path: `HAVING row_number() OVER () = 1` has no
        // window in its projection at all.
        crate::window::reject_window_in_row_clauses(stmt)?;
        // v7.39 (round 232) — the ORDER BY legality rules (positional
        // bounds, DISTINCT, DISTINCT ON). Same placement as the window
        // check: before anything scans.
        crate::orderby::check_order_by_legality(stmt)?;
        // v7.37.16 — resolve `USING` column-merge + `NATURAL JOIN` into an
        // equivalent statement the regular executor handles (merged join
        // columns collapse to a single unqualified output column; NATURAL
        // gets its common-column ON synthesised). The rewrite clears the
        // flags, so this re-entrant call is a no-op on the second pass.
        if let Some(rewritten) = self.desugar_using_natural(stmt)? {
            return self.exec_bare_select_cancel(&rewritten, cancel);
        }
        // v7.39 (RLS) Phase 3 — cross-table joins: wrap each RLS-enabled join
        // operand in a security-barrier subquery, then re-enter (the wrapped
        // operands are no longer bare RLS tables, so this is a no-op on the
        // second pass).
        if let Some(rewritten) = self.rls_rewrite_joins(stmt) {
            return self.exec_bare_select_cancel(&rewritten, cancel);
        }
        // v7.39 (RLS) Phase 1 — for a policy-subject (non-superuser) session,
        // AND the RLS USING predicate into a single-table SELECT's WHERE.
        // Superuser sessions and non-RLS tables get `None` (no clone, no
        // change). Applied inline (shadowing `stmt`) rather than via re-entry
        // so it can't re-inject on a recursive pass.
        let rls_stmt;
        let stmt = match self.rls_select_predicate(stmt)? {
            Some(pred) => {
                let mut s = stmt.clone();
                s.where_ = Some(match s.where_.take() {
                    Some(existing) => spg_sql::ast::Expr::Binary {
                        lhs: alloc::boxed::Box::new(existing),
                        op: spg_sql::ast::BinOp::And,
                        rhs: alloc::boxed::Box::new(pred),
                    },
                    None => pred,
                });
                rls_stmt = s;
                &rls_stmt
            }
            None => stmt,
        };
        // v7.16.2 — same meta-view dispatch as
        // `exec_select_cancel`, applied here too because
        // `subquery_replacement` enters this function directly
        // for Exists / ScalarSubquery / InSubquery resolution
        // (bypassing the top-level entry to avoid double
        // subquery walking). Without this dispatch the subquery
        // hits `__spg_info_columns` and reports TableNotFound.
        if let Some(done) = self.try_pre_from_paths(stmt, cancel)? {
            return Ok(done);
        }
        // Constant SELECT (no FROM) — evaluate each item once against an
        // empty dummy row. Useful for `SELECT 1`, `SELECT coalesce(...)`,
        // `SELECT '7'::INT`. Column references will surface as
        // ColumnNotFound on eval since the schema is empty.
        let Some(from) = &stmt.from else {
            return self.exec_constant_select(stmt);
        };
        // Multi-table FROM (one or more joined peers) goes through the
        // nested-loop join executor. Single-table FROM stays on the
        // existing scan + index-seek path.
        if let Some(done) = self.try_from_shape_paths(stmt, from, cancel)? {
            return Ok(done);
        }
        // NOT hooked up. `try_spill_sorted_scan` is written, correct and
        // tested — eight ORDER BY shapes byte-identical spilled against
        // in-memory, with 103 runs opened to prove the spill ran — and it
        // loses on wall clock, which is a hard stop whatever the memory
        // buys. Measured round 865, same psql client both sides, same
        // machine, row counts verified, and both sides confirmed to be
        // doing an external merge rather than an indexed walk:
        //
        //   PG18        178.7 - 187.0 ms   Sort Method: external merge, 85 MB
        //   SPG spilled 269.7 - 299.6 ms   33 spill files at peak
        //
        // Non-overlapping, about 1.55x. Re-enable by restoring the call
        // below once that closes; nothing else has to change, which is
        // the point of it being a separate path.
        //
        //   if let Some(done) = self.try_spill_sorted_scan(stmt, from, cancel)? {
        //       return Ok(done);
        //   }
        //
        // v7.37 (round 882) — this walk stays unhooked, but its streaming
        // twin `try_spill_sorted_stream` IS hooked, above the ORDER BY
        // bail in `try_exec_joined_streaming`. Collecting the answer was
        // most of what this one cost: handing rows over as the merge
        // produces them holds peak to the budget plus one row, and the
        // wall clock lands inside PG18's range rather than 1.55x outside
        // it. Numbers in `extsort.rs`'s header.
        let primary = &from.primary;
        // v7.39 (round 244) — a sequence is selectable as a one-row relation
        // in PG (`SELECT last_value FROM seq` — psql's \d and several ORMs
        // read it). Synthesize PG's three columns.
        if let Some(done) = self.try_sequence_relation(stmt, primary, cancel)? {
            return Ok(done);
        }
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let schema_cols = &table.schema().columns;
        // The qualifier accepted on column refs is the alias (if any) else the
        // bare table name.
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        // v7.39 (round 511) — `ctid`, PG's physical row identity. SPG had no
        // system columns at all: `SELECT ctid FROM t` answered "column
        // \"ctid\" does not exist", which takes out the dedup idiom every
        // PG user knows — `DELETE … WHERE ctid NOT IN (SELECT min(ctid) …
        // GROUP BY key)`.
        //
        // The value comes from the row's position, which the scan already
        // yields; the column is appended to the schema and the rows only
        // when the statement asks for it, so nothing else pays for it. That
        // also routes the query down the general path, past the index fast
        // paths below — they hand back rows without positions, and a ctid
        // that was sometimes right would be worse than none.
        if let Some(done) =
            self.try_ctid_projection(stmt, primary, table, schema_cols, alias, cancel)?
        {
            return Ok(done);
        }
        let ctx = self.ev_ctx(schema_cols, Some(alias));

        // NSW kNN planner: `ORDER BY col <-> literal LIMIT k` with no
        // WHERE and an NSW index on `col` skips the full scan. The
        // walk returns rows already in ascending-distance order, so
        // ORDER BY / LIMIT are honoured implicitly.
        // Phase C.3 step 2c — compute the reader's MVCC snapshot once
        // and thread it into every index-seek fast path below. No-op
        // today (every hot header is committed-alive).
        let seek_snapshot = self.current_snapshot();
        if let Some(done) =
            self.try_seek_fast_paths(stmt, table, schema_cols, alias, &seek_snapshot, cancel)?
        {
            return Ok(done);
        }
        // full scan over the hot tier (cold-tier rows are only reached
        // via index seek in v5.1 — full table scans against cold-tier
        // data ship in v5.2 with the freezer's per-segment scan API).
        let indexed_rows =
            self.pick_indexed_rows(stmt, table, schema_cols, alias, &ctx, &seek_snapshot);

        // Aggregate path: filter rows first, then hand off to the
        // aggregate executor which does its own projection + ORDER BY.
        if aggregate::uses_aggregate(stmt) {
            return self.run_single_table_aggregate(
                stmt,
                table,
                schema_cols,
                alias,
                indexed_rows,
                cancel,
            );
        }
        self.run_single_table_scan(stmt, table, schema_cols, alias, indexed_rows, cancel)
    }

    /// v7.37.43-T4.5 — execute `SELECT … FROM jsonb_each_text(<expr>)`.
    /// Sentori migration 0067 uses this with `CROSS JOIN LATERAL`; the
    /// uncorrelated FROM-primary case is the simpler shape, used by
    /// e2e pins. Materialises the (key, value) pair stream into a
    /// synthetic two-column TEXT table, then routes through the
    /// regular projection / WHERE / ORDER BY pipeline.
    /// v7.39 (read01 partitionfuncs.c) — materialise a FROM-position
    /// v7.39 (round 205, JSON_TABLE) — materialise a JSON_TABLE FROM
    /// item into (rows, schema). `outer_doc` is `Some` only when this
    /// is a NESTED level being expanded against a parent row item's
    /// already-parsed sub-document; the top-level call parses the doc
    /// expr itself. Row/column paths reuse the existing jsonpath
    /// evaluator (`json::json_table_path`); coercion reuses
    /// `coerce_value` on the JSON scalar text, so a json string
    /// coerces to DATE by its content, matching PG.
    #[allow(clippy::type_complexity)]
    pub(crate) fn json_table_rows(
        &self,
        jt: &spg_sql::ast::JsonTable,
        outer_doc: Option<&crate::json::JsonValue>,
    ) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<ColumnSchema>), EngineError> {
        // Column schema is static (independent of data): flatten the
        // COLUMNS tree in declaration order (NESTED contributes its
        // children inline, the PG output shape).
        let schema = json_table_schema(&jt.columns);

        // PASSING variables → a single JsonValue object the jsonpath
        // engine reads `$name` from.
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy = Row::new(alloc::vec::Vec::new());
        let vars: Option<crate::json::JsonValue> = if jt.passing.is_empty() {
            None
        } else {
            let mut entries = alloc::vec::Vec::new();
            for (name, e) in &jt.passing {
                let v = eval::eval_expr(e, &dummy, &ctx).map_err(EngineError::Eval)?;
                entries.push((name.clone(), value_to_json_value(&v)));
            }
            Some(crate::json::JsonValue::Object(entries))
        };

        // The document root: a NESTED level gets it from the parent;
        // the top level parses its doc expr.
        let root_owned;
        let root: &crate::json::JsonValue = match outer_doc {
            Some(d) => d,
            None => {
                let doc_val = eval::eval_expr(&jt.doc, &dummy, &ctx).map_err(EngineError::Eval)?;
                let src = match &doc_val {
                    Value::Null => return Ok((alloc::vec::Vec::new(), schema)),
                    Value::Json(s) | Value::Text(s) => s.as_ref().to_string(),
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "JSON_TABLE document must be json/text, got {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        )));
                    }
                };
                root_owned = crate::json::parse_doc(&src).map_err(EngineError::Eval)?;
                &root_owned
            }
        };

        let items = crate::json::json_table_path(root, &jt.row_path, vars.as_ref())
            .map_err(EngineError::Eval)?;
        let mut rows: alloc::vec::Vec<Row<'static>> = alloc::vec::Vec::new();
        for (idx, item) in items.iter().enumerate() {
            self.json_table_emit_item(jt, item, idx, vars.as_ref(), &mut rows)?;
        }
        Ok((rows, schema))
    }

    /// v7.39 (round 205) — emit the row(s) for one row-pattern item.
    /// Regular columns produce one value each; a NESTED column expands
    /// as an outer join (each nested match → one row sharing the
    /// parent cells; no nested match → one row with the nested cells
    /// NULL). Sibling NESTED at one level cross by concatenation of
    /// their independent expansions (PG's UNION-of-outer shape).
    fn json_table_emit_item(
        &self,
        jt: &spg_sql::ast::JsonTable,
        item: &crate::json::JsonValue,
        ordinality: usize,
        vars: Option<&crate::json::JsonValue>,
        out: &mut alloc::vec::Vec<Row<'static>>,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::JsonTableColumn as C;
        // Parent cells (regular + ordinality), left-to-right; NESTED
        // columns contribute a run of child cells appended after.
        let mut parent_cells: alloc::vec::Vec<Value<'static>> = alloc::vec::Vec::new();
        let mut nested_runs: alloc::vec::Vec<alloc::vec::Vec<Row<'static>>> =
            alloc::vec::Vec::new();
        let mut nested_widths: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        for col in &jt.columns {
            match col {
                C::Ordinality { .. } => {
                    parent_cells.push(Value::BigInt(ordinality as i64 + 1));
                }
                C::Regular { .. } => {
                    parent_cells.push(self.json_table_column_value(col, item, vars)?);
                }
                C::Nested { path, columns } => {
                    // Recurse: a nested JSON_TABLE over `item` filtered
                    // by `path`, with the same PASSING vars.
                    let sub = spg_sql::ast::JsonTable {
                        doc: jt.doc.clone(), // unused (outer_doc provided)
                        row_path: path.clone(),
                        columns: columns.clone(),
                        passing: alloc::vec::Vec::new(),
                    };
                    let (nrows, nschema) = self.json_table_rows(&sub, Some(item))?;
                    nested_widths.push(nschema.len());
                    nested_runs.push(nrows);
                }
            }
        }
        if nested_runs.is_empty() {
            out.push(Row::new(parent_cells));
            return Ok(());
        }
        // PG sibling-NESTED semantics: each sibling expands
        // INDEPENDENTLY and the results CONCATENATE — a row from
        // sibling s fills only s's cells, every other sibling's cells
        // NULL. An empty sibling contributes ZERO rows (not a NULL
        // row). Only when EVERY sibling is empty does the parent still
        // emit one all-NULL row (the outer-join guarantee that a parent
        // item is never dropped). Verified vs PG18 (r207): a=1,b=2 → 3
        // rows; a=1,b=[] → 1 row; all-empty → 1 NULL row.
        let before = out.len();
        for (s_idx, run) in nested_runs.iter().enumerate() {
            for nrow in run {
                let mut cells = parent_cells.clone();
                for (o_idx, w) in nested_widths.iter().enumerate() {
                    if o_idx == s_idx {
                        cells.extend(nrow.values.iter().cloned());
                    } else {
                        for _ in 0..*w {
                            cells.push(Value::Null);
                        }
                    }
                }
                out.push(Row::new(cells));
            }
        }
        if out.len() == before {
            // Every sibling empty → one all-NULL nested row.
            let mut cells = parent_cells.clone();
            for w in &nested_widths {
                for _ in 0..*w {
                    cells.push(Value::Null);
                }
            }
            out.push(Row::new(cells));
        }
        Ok(())
    }

    /// v7.39 (round 205) — evaluate one Regular column against a row
    /// item: EXISTS → bool; else path → at most one value, coerced to
    /// the declared type with ON EMPTY / ON ERROR / DEFAULT behaviour.
    fn json_table_column_value(
        &self,
        col: &spg_sql::ast::JsonTableColumn,
        item: &crate::json::JsonValue,
        vars: Option<&crate::json::JsonValue>,
    ) -> Result<Value<'static>, EngineError> {
        use spg_sql::ast::{JsonTableColumn as C, JsonTableOnBehavior as B};
        let C::Regular {
            name,
            ty,
            path,
            exists,
            format_json,
            wrapper,
            on_empty,
            on_error,
        } = col
        else {
            unreachable!("caller guards Regular");
        };
        let matches = crate::json::json_table_path(item, path, vars).map_err(EngineError::Eval)?;
        if *exists {
            return Ok(Value::Bool(!matches.is_empty()));
        }
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy = Row::new(alloc::vec::Vec::new());
        let default_of = |b: &B| -> Result<Option<Value<'static>>, EngineError> {
            match b {
                B::Null => Ok(Some(Value::Null)),
                B::Error => Ok(None),
                B::Default(e) => Ok(Some(
                    eval::eval_expr(e, &dummy, &ctx).map_err(EngineError::Eval)?,
                )),
            }
        };
        // Empty match set → ON EMPTY.
        if matches.is_empty() {
            return match default_of(on_empty)? {
                Some(v) => coerce_json_table_default(v, *ty, name),
                None => Err(EngineError::Unsupported(alloc::format!(
                    "no SQL/JSON item found for JSON_TABLE column {name:?}"
                ))),
            };
        }
        let first = &matches[0];
        // FORMAT JSON: return the PG-canonical json representation.
        // WITH WRAPPER wraps the whole match SET in an array (even a
        // single scalar → `[5]`); without it, the single match's json.
        if *format_json {
            let text = if *wrapper {
                crate::json::JsonValue::Array(matches.clone()).canonical_json_text()
            } else {
                first.canonical_json_text()
            };
            return Ok(Value::Json(alloc::borrow::Cow::Owned(text)));
        }
        if first.is_json_null() {
            return Ok(Value::Null);
        }
        // Coerce the scalar text to the declared type; on failure → ON
        // ERROR (default NULL, DEFAULT expr, or raise).
        let dt = crate::conversions::column_type_to_data_type(*ty);
        let scalar = Value::Text(alloc::borrow::Cow::Owned(first.scalar_text()));
        match crate::conversions::coerce_value(scalar, dt, name, 0) {
            Ok(v) => Ok(v),
            Err(e) => match default_of(on_error)? {
                Some(v) => coerce_json_table_default(v, *ty, name),
                None => Err(e),
            },
        }
    }

    /// table function into (rows, default schema). Dispatch by name.
    pub(crate) fn table_fn_rows(
        &self,
        primary: &TableRef,
    ) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<ColumnSchema>), EngineError> {
        let (fn_name, args) = primary
            .table_fn_call
            .as_deref()
            .expect("caller guards table_fn_call.is_some()");
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        let arg0: Option<Value<'static>> = match args.first() {
            Some(e) => Some(eval::eval_expr(e, &dummy_row, &ctx).map_err(EngineError::Eval)?),
            None => None,
        };
        match fn_name.as_str() {
            // v7.39 (read01 round 76) — `jsonb_populate_record(NULL::t, j)` /
            // `…_recordset` (+ json_ variants). The row shape is the BASE
            // argument's declared type — a table's or a composite type's
            // column list — which only the catalog knows, so the parser hands
            // the raw arguments here rather than desugaring blind.
            "jsonb_populate_record"
            | "json_populate_record"
            | "jsonb_populate_recordset"
            | "json_populate_recordset" => {
                let type_name = match args.first() {
                    Some(Expr::Cast {
                        target: spg_sql::ast::CastTarget::Named(n),
                        ..
                    }) => n.clone(),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "{fn_name}(): first argument must name a row type, \
                             e.g. NULL::mytable"
                        )));
                    }
                };
                let cat = self.active_catalog();
                let cols: alloc::vec::Vec<ColumnSchema> = if let Some(t) = cat.get(&type_name) {
                    t.schema().columns.clone()
                } else if let Some(c) = cat.composite_types().get(&type_name) {
                    c.fields
                        .iter()
                        .map(|(n, ty)| ColumnSchema::new(n.clone(), *ty, true))
                        .collect()
                } else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "type \"{type_name}\" does not exist"
                    )));
                };
                let json_arg = match args.get(1) {
                    Some(e) => eval::eval_expr(e, &dummy_row, &ctx).map_err(EngineError::Eval)?,
                    None => Value::Null,
                };
                // The set form iterates the JSON array; the scalar form is
                // the one-element case of the same walk.
                let docs: alloc::vec::Vec<Value<'static>> = if fn_name.ends_with("recordset") {
                    crate::json::array_element_rows(&json_arg, false, fn_name)
                        .map_err(EngineError::Eval)?
                        .into_iter()
                        .map(|s| s.map_or(Value::Null, Value::json))
                        .collect()
                } else if matches!(json_arg, Value::Null) {
                    alloc::vec::Vec::new()
                } else {
                    alloc::vec![json_arg]
                };
                let mut rows = alloc::vec::Vec::with_capacity(docs.len());
                for doc in &docs {
                    let mut vals = alloc::vec::Vec::with_capacity(cols.len());
                    for c in &cols {
                        // `->>` semantics: a missing key is NULL, present keys
                        // arrive as text and cast to the declared column type.
                        let raw = crate::json::path_get(doc, &Value::text(c.name.clone()), true)
                            .map_err(EngineError::Eval)?;
                        let v = if matches!(raw, Value::Null) {
                            Value::Null
                        } else {
                            crate::conversions::coerce_value(raw, c.ty, "", 0)
                                .map_err(|e| EngineError::Unsupported(alloc::format!("{e:?}")))?
                        };
                        vals.push(v);
                    }
                    rows.push(Row::new(vals));
                }
                Ok((rows, cols))
            }
            "pg_partition_tree" => {
                let cols = alloc::vec![
                    ColumnSchema::new("relid".to_string(), DataType::Text, true),
                    ColumnSchema::new("parentrelid".to_string(), DataType::Text, true),
                    ColumnSchema::new("isleaf".to_string(), DataType::Bool, true),
                    ColumnSchema::new("level".to_string(), DataType::Int, true),
                ];
                let Some(Value::Text(name)) = &arg0 else {
                    // NULL (or missing) argument → zero rows (PG).
                    return Ok((alloc::vec::Vec::new(), cols));
                };
                let entries = crate::partition_walks::tree_of(self.active_catalog(), name.as_ref());
                if entries.is_empty() && self.active_catalog().get(name.as_ref()).is_none() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation \"{name}\" does not exist"
                    )));
                }
                let rows = entries
                    .into_iter()
                    .map(|(relid, parent, isleaf, level)| {
                        Row::new(alloc::vec![
                            Value::text(relid),
                            parent.map_or(Value::Null, Value::text),
                            Value::Bool(isleaf),
                            #[allow(clippy::cast_possible_truncation)]
                            Value::Int(level as i32),
                        ])
                    })
                    .collect();
                Ok((rows, cols))
            }
            "pg_partition_ancestors" => {
                let cols =
                    alloc::vec![ColumnSchema::new("relid".to_string(), DataType::Text, true)];
                let Some(Value::Text(name)) = &arg0 else {
                    return Ok((alloc::vec::Vec::new(), cols));
                };
                let cat = self.active_catalog();
                if cat.get(name.as_ref()).is_none() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation \"{name}\" does not exist"
                    )));
                }
                // A relation outside any partition tree yields no rows (PG).
                let in_tree = cat
                    .get(name.as_ref())
                    .is_some_and(|t| t.schema().partition_role.is_some());
                let rows = if in_tree {
                    crate::partition_walks::ancestors_of(cat, name.as_ref())
                        .into_iter()
                        .map(|n| Row::new(alloc::vec![Value::text(n)]))
                        .collect()
                } else {
                    alloc::vec::Vec::new()
                };
                Ok((rows, cols))
            }
            // v7.39 (round 651) — `ts_debug(config, text)`: what the parser
            // saw, what each token was called, which dictionary took it
            // and what came out. It is a projection of the same tokenizer
            // and the same map the indexer uses, so it cannot describe a
            // pipeline other than the one that runs.
            "ts_debug" => {
                use crate::fts::{TokenType, TsDict};
                let cols = alloc::vec![
                    ColumnSchema::new("alias".to_string(), DataType::Text, false),
                    ColumnSchema::new("description".to_string(), DataType::Text, false),
                    ColumnSchema::new("token".to_string(), DataType::Text, false),
                    ColumnSchema::new("dictionaries".to_string(), DataType::TextArray, false),
                    ColumnSchema::new("dictionary".to_string(), DataType::Text, true),
                    ColumnSchema::new("lexemes".to_string(), DataType::TextArray, true),
                ];
                // PG's one-arg form uses the session configuration; the
                // two-arg form names one.
                let (cfg_name, text) = match (&arg0, args.get(1)) {
                    (Some(Value::Text(c)), Some(t)) => {
                        let v = eval::eval_expr(t, &dummy_row, &ctx).map_err(EngineError::Eval)?;
                        (c.to_string(), crate::eval::value_to_text(&v))
                    }
                    (Some(v), None) => (
                        alloc::string::String::from("english"),
                        crate::eval::value_to_text(v),
                    ),
                    _ => return Ok((alloc::vec::Vec::new(), cols)),
                };
                let english = match cfg_name
                    .trim()
                    .trim_start_matches("pg_catalog.")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "english" => true,
                    "simple" => false,
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "text search configuration \"{other}\" does not exist"
                        )));
                    }
                };
                let rows = crate::fts::tokenize_typed(&text)
                    .into_iter()
                    .map(|tok| {
                        let dict = tok.ty.dictionary(english);
                        let dname = dict.map(|d| match d {
                            TsDict::Simple => "simple",
                            TsDict::EnglishStem => "english_stem",
                        });
                        let folded = tok.text.to_lowercase();
                        let lexemes = dict.map(|d| match d {
                            TsDict::Simple => alloc::vec![Some(folded.clone())],
                            TsDict::EnglishStem => {
                                if crate::fts::is_english_stopword(&folded) {
                                    alloc::vec::Vec::new()
                                } else {
                                    alloc::vec![Some(crate::fts::porter_stem(&folded))]
                                }
                            }
                        });
                        Row::new(alloc::vec![
                            Value::text(tok.ty.alias()),
                            Value::text(tok.ty.description()),
                            Value::text(tok.text),
                            Value::TextArray(
                                dname
                                    .map(|n| alloc::vec![Some(alloc::string::String::from(n))])
                                    .unwrap_or_default(),
                            ),
                            dname.map_or(Value::Null, Value::text),
                            lexemes.map_or(Value::Null, Value::TextArray),
                        ])
                    })
                    .collect();
                let _ = TokenType::AsciiWord;
                Ok((rows, cols))
            }
            // v7.39 (round 651) — `ts_token_type('default')`, the list the
            // parser actually produces. It is a projection of the
            // `TokenType` enum the tokenizer and `pg_ts_config_map` both
            // read, so the three cannot disagree about what a token is.
            "ts_token_type" => {
                use crate::fts::TokenType as T;
                let cols = alloc::vec![
                    ColumnSchema::new("tokid".to_string(), DataType::Int, false),
                    ColumnSchema::new("alias".to_string(), DataType::Text, false),
                    ColumnSchema::new("description".to_string(), DataType::Text, false),
                ];
                // PG takes the parser by name or oid; SPG has the one.
                if let Some(Value::Text(p)) = &arg0
                    && !p.eq_ignore_ascii_case("default")
                    && !p.eq_ignore_ascii_case("pg_catalog.default")
                {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "text search parser \"{p}\" does not exist"
                    )));
                }
                const TYPES: &[T] = &[
                    T::AsciiWord,
                    T::Word,
                    T::NumWord,
                    T::Email,
                    T::Url,
                    T::Host,
                    T::SFloat,
                    T::Version,
                    T::HwordNumPart,
                    T::HwordPart,
                    T::HwordAsciiPart,
                    T::Blank,
                    T::Tag,
                    T::Protocol,
                    T::NumHword,
                    T::AsciiHword,
                    T::Hword,
                    T::UrlPath,
                    T::File,
                    T::Float,
                    T::Int,
                    T::Uint,
                    T::Entity,
                ];
                let rows = TYPES
                    .iter()
                    .map(|t| {
                        Row::new(alloc::vec![
                            Value::Int(*t as i32),
                            Value::text(t.alias()),
                            Value::text(t.description()),
                        ])
                    })
                    .collect();
                Ok((rows, cols))
            }
            // v7.39 (read01 round 65) — a set-returning USER function in FROM
            // (`FROM rows_of(2)`). Its body runs through the real executor, like
            // every other function body since round 63.
            other => {
                if !self.active_catalog().functions_named(other).is_empty() {
                    return self.exec_setof_user_function(other, args, primary.alias.as_deref());
                }
                Err(EngineError::Unsupported(alloc::format!(
                    "table function {other}() is not supported in FROM"
                )))
            }
        }
    }

    /// v7.39 (read01 round 65) — run a `RETURNS SETOF <type>` / `RETURNS
    /// TABLE(…)` function in FROM position. The body is a SELECT; the arguments
    /// are bound into it as literals and it goes through the read path, so the
    /// rows it yields are exactly the rows a hand-written query would see.
    ///
    /// The column NAMES come from the declared shape: `RETURNS TABLE(id int, v
    /// text)` names them, and a `SETOF <scalar>` yields a single column named
    /// after the function — PG's rule, and what a bare `SELECT * FROM f()`
    /// shows.
    fn exec_setof_user_function(
        &self,
        name: &str,
        args: &[spg_sql::ast::Expr],
        // v7.39 (read01 round 65) — `FROM evens() AS x` names the single column
        // `x`: for a scalar SETOF, the table alias IS the column name (PG).
        alias: Option<&str>,
    ) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<ColumnSchema>), EngineError> {
        // The call's arguments belong to the ENCLOSING query, so they are
        // evaluated here and the body sees values.
        let empty: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let arg_ctx = self.ev_ctx(&empty, None);
        let dummy = Row::new(alloc::vec::Vec::new());
        let mut vals: alloc::vec::Vec<Value<'static>> = alloc::vec::Vec::new();
        for a in args {
            vals.push(eval::eval_expr(a, &dummy, &arg_ctx).map_err(EngineError::Eval)?);
        }
        self.setof_rows_of(name, &vals, alias)
    }

    /// v7.39 (read01 round 67) — the set-returning core, on already-evaluated
    /// arguments. Shared by the FROM position and the target-list expansion, so
    /// a function cannot behave differently depending on where it is called.
    pub(crate) fn setof_rows_of(
        &self,
        name: &str,
        arg_values: &[Value<'static>],
        alias: Option<&str>,
    ) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<ColumnSchema>), EngineError> {
        let cat = self.active_catalog();
        let overloads = cat.functions_named(name);
        let def = overloads
            .iter()
            .find(|f| spg_storage::function_arg_types(&f.args_repr).len() == arg_values.len())
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "function {name} does not exist with {} argument(s)",
                    arg_values.len()
                ))
            })?;
        let declared = def.returns.trim().to_string();
        let upper = declared.to_ascii_uppercase();
        if !upper.starts_with("SETOF") && !upper.starts_with("TABLE(") {
            return Err(EngineError::Unsupported(alloc::format!(
                "function {name}() does not return a set — it cannot be used in FROM"
            )));
        }

        let arg_names_pl = spg_storage::function_arg_names(&def.args_repr);
        // v7.39 (read01 round 66) — a plpgsql SETOF body builds its rows with
        // RETURN NEXT / RETURN QUERY; the interpreter collects them.
        if def.language.eq_ignore_ascii_case("plpgsql") {
            let out_rows = self
                .call_plpgsql_setof_fn(def, &arg_names_pl, arg_values)
                .map_err(EngineError::Eval)?;
            let cols = setof_column_shape(&declared, name, alias, out_rows.first());
            let rows = out_rows.into_iter().map(Row::new).collect();
            return Ok((rows, cols));
        }
        let body = def.body.trim().trim_end_matches(';');
        let stmt = spg_sql::parser::parse_statement(body).map_err(|e| {
            EngineError::Unsupported(alloc::format!("function {name} body does not parse: {e}"))
        })?;
        let spg_sql::ast::Statement::Select(body_select) = stmt else {
            return Err(EngineError::Unsupported(alloc::format!(
                "function {name}(): a set-returning body must be a SELECT"
            )));
        };
        let arg_names = spg_storage::function_arg_names(&def.args_repr);
        let bound = crate::eval::bind_user_fn_args(
            self.active_catalog(),
            &body_select,
            &arg_names,
            arg_values,
        )
        .map_err(EngineError::Eval)?;
        let out = self.exec_select_cancel(&bound, crate::CancelToken::none())?;
        let QueryResult::Rows { columns, rows } = out else {
            return Ok((alloc::vec::Vec::new(), alloc::vec::Vec::new()));
        };
        // Name the columns from the DECLARED shape — the same rule the plpgsql
        // path above uses, so a body's language cannot change the row shape.
        let cols = setof_column_shape_from(&declared, name, alias, &columns);
        Ok((rows, cols))
    }

    fn exec_select_jsonb_each_text(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let (each_fn, arg_expr) = primary
            .jsonb_each_text_arg
            .as_ref()
            .map(|(name, expr)| (name.as_str(), expr.as_ref()))
            .expect("caller guards jsonb_each_text_arg.is_some()");
        // v7.37.17 (17.6 siblings) — the plain jsonb_each / json_each
        // forms keep JSON rendering in the value column (JSON null
        // stays jsonb 'null', strings keep their quotes).
        let as_text = each_fn.ends_with("_text");
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        let arg_value = eval::eval_expr(arg_expr, &dummy_row, &ctx).map_err(EngineError::Eval)?;
        let pairs =
            crate::json::each_rows(&arg_value, as_text, each_fn).map_err(EngineError::Eval)?;
        let rows: alloc::vec::Vec<Row<'static>> = pairs
            .into_iter()
            .map(|(k, v)| {
                let key_val = Value::text(k);
                let value_val = match v {
                    Some(s) if as_text => Value::text(s),
                    Some(s) => Value::Json(alloc::borrow::Cow::Owned(s)),
                    None => Value::Null,
                };
                Row::new(alloc::vec![key_val, value_val])
            })
            .collect();
        let alias = primary.alias.clone().unwrap_or_else(|| each_fn.to_string());
        let value_dtype = if as_text {
            spg_storage::DataType::Text
        } else {
            spg_storage::DataType::Json
        };
        let key_col = ColumnSchema::new("key".to_string(), spg_storage::DataType::Text, false);
        let value_col = ColumnSchema::new("value".to_string(), value_dtype, as_text);
        let mut schema_cols = alloc::vec![key_col, value_col];
        // `AS t(k, v)` renames key/value positionally (PG behaviour); the
        // LATERAL-position form of the same call already honours it.
        for (i, new_name) in primary.unnest_column_aliases.iter().enumerate() {
            if let Some(col) = schema_cols.get_mut(i) {
                col.name = new_name.clone();
            }
        }
        // v7.39 (read01 round 54) — `ev_ctx` threads the catalog; a bare
        // `EvalContext::new` drops it and every catalog-dependent cast
        // (regclass / enum / composite / domain) silently degrades.
        let scan_ctx = self.ev_ctx(&schema_cols, Some(&alias));
        // WHERE.
        let filtered: alloc::vec::Vec<Row<'static>> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = eval::eval_expr(w, &row, &scan_ctx).map_err(EngineError::Eval)?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // Aggregate dispatch (e.g. SELECT COUNT(*) FROM jsonb_each_text…).
        if aggregate::uses_aggregate(stmt) {
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            // v7.39 (round 656) — hand the rows over as they are rather than
            // collecting a second vector of `RowRef` wrappers. Note this is
            // a set-returning-function path, NOT the relational scan: the
            // measured O(rows) cost lived in `run_single_table_aggregate`,
            // and converting these four first was a miss that cost a full
            // round — every test stayed green and the number did not move.
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Owned(&filtered),
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection =
            build_projection(&stmt.items, &schema_cols, &alias, self.backslash_escapes)?;
        let mut projected_rows: alloc::vec::Vec<Row<'static>> =
            alloc::vec::Vec::with_capacity(filtered.len());
        for row in &filtered {
            let mut vals = alloc::vec::Vec::with_capacity(projection.len());
            for p in &projection {
                let v = eval::eval_expr(&p.expr, row, &scan_ctx).map_err(EngineError::Eval)?;
                vals.push(v);
            }
            projected_rows.push(Row::new(vals));
        }
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            // v7.39 (read01 round 54) — keep the column's enum identity through
            // the projection (it lives outside the DataType lattice), or a
            // derived table / UNION / windowed result forgets it and any outer
            // `ORDER BY <enum col>` silently sorts by the label's TEXT.
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        // ORDER BY.
        if !stmt.order_by.is_empty() {
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value<'static>>)> = filtered
                .iter()
                .enumerate()
                .map(|(i, r)| -> Result<_, EngineError> {
                    let keys: Result<Vec<Value<'static>>, EngineError> = stmt
                        .order_by
                        .iter()
                        .map(|ob| {
                            eval::eval_expr(&ob.expr, r, &scan_ctx).map_err(EngineError::Eval)
                        })
                        .collect();
                    Ok((i, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &stmt.order_by[idx];
                    let cmp = order_by_value_cmp_in(
                        o.desc,
                        o.nulls_first,
                        ka,
                        kb,
                        scan_ctx.mysql_dialect && !crate::eval::is_binary_coerced(&o.expr),
                    );
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        // v7.38 (read01) — DISTINCT over a synthetic source was dropped here.
        if stmt.distinct {
            projected_rows = dedup_rows(projected_rows, scan_ctx.mysql_dialect);
        }
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    /// v7.37.17 (17.6 siblings) — execute `SELECT … FROM
    /// ( SELECT … ) alias` in primary position. The inner SELECT
    /// materialises once through the regular bare-select executor
    /// (UNION tails included), then the outer WHERE / aggregate /
    /// projection / ORDER BY / LIMIT pipeline runs over the
    /// synthetic table — the same post-materialisation shape as
    /// exec_select_jsonb_each_text, generalised to N columns.
    fn exec_select_derived(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let inner = primary
            .lateral_subquery
            .as_deref()
            .expect("caller guards lateral_subquery.is_some()");
        // exec_select_cancel is the union-aware wrapper — the inner
        // SELECT may carry UNION tails on stmt.unions.
        let QueryResult::Rows {
            columns: inner_cols,
            rows,
        } = self.exec_select_cancel(inner, cancel)?
        else {
            return Err(EngineError::Unsupported(
                "derived table subquery must return rows".into(),
            ));
        };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| primary.name.clone());
        // `AS t(a, b)` renames the materialised columns positionally
        // (extra inner columns keep their own names, PG behaviour).
        let mut schema_cols: alloc::vec::Vec<ColumnSchema> = inner_cols;
        // v7.39 (read01 round 78) — a column-alias list longer than the item is
        // the error PG reports; SPG used to let the extra names through and then
        // fail two layers downstream with "column not found: <the extra name>".
        let n_out = schema_cols.len() + usize::from(primary.with_ordinality);
        if primary.unnest_column_aliases.len() > n_out {
            return Err(EngineError::Unsupported(alloc::format!(
                "table \"{alias}\" has {n_out} columns available but {} columns specified",
                primary.unnest_column_aliases.len()
            )));
        }
        if primary.scalar_fn_item && schema_cols.len() == 1 {
            schema_cols[0].scalar_row_source = true;
        }
        // v7.39 (read01 round 78) — WITH ORDINALITY on a table function that
        // rides this channel (regexp_matches): a trailing bigint counter, 1-based.
        // The column-alias list, if given, names it like any other column.
        let mut rows = rows;
        if primary.with_ordinality {
            schema_cols.push(ColumnSchema::new(
                "ordinality".to_string(),
                DataType::BigInt,
                false,
            ));
            rows = rows
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut v = r.values;
                    #[allow(clippy::cast_possible_wrap)]
                    v.push(Value::BigInt(i as i64 + 1));
                    Row::new(v)
                })
                .collect();
        }
        for (i, new_name) in primary.unnest_column_aliases.iter().enumerate() {
            if let Some(col) = schema_cols.get_mut(i) {
                col.name = new_name.clone();
            }
        }
        self.exec_select_over_rows(stmt, rows, schema_cols, &alias, cancel)
    }

    /// v7.39 (read01 partitionfuncs.c) — shared synthetic-source SELECT
    /// pipeline (WHERE / aggregate / projection / ORDER BY / DISTINCT /
    /// OFFSET / LIMIT) over a pre-materialised row set. Drives the
    /// derived-table executor and the FROM-position table functions.
    fn exec_select_over_rows(
        &self,
        stmt: &SelectStatement,
        rows: alloc::vec::Vec<Row<'static>>,
        schema_cols: alloc::vec::Vec<ColumnSchema>,
        alias: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let scan_ctx = self.ev_ctx(&schema_cols, Some(alias));
        // v7.37 D.21 — correlated subqueries in the WHERE / projection may
        // reference this derived table's columns (`… WHERE u.gg = t.g` where t
        // is `(VALUES …) t`). Resolve them per-row via eval_expr_with_correlated
        // (the same path the aggregate branch uses); the old plain eval_expr let
        // a ScalarSubquery reach row-eval unresolved ("engine resolver bug").
        let corr_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
        // WHERE.
        let filtered: alloc::vec::Vec<Row<'static>> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = self.eval_expr_with_correlated(
                    w,
                    &row,
                    &scan_ctx,
                    cancel,
                    Some(&mut corr_memo.borrow_mut()),
                )?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // Aggregate dispatch.
        if aggregate::uses_aggregate(stmt) {
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            // v7.39 (round 656) — hand the rows over as they are rather than
            // collecting a second vector of `RowRef` wrappers. Note this is
            // a set-returning-function path, NOT the relational scan: the
            // measured O(rows) cost lived in `run_single_table_aggregate`,
            // and converting these four first was a miss that cost a full
            // round — every test stayed green and the number did not move.
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Owned(&filtered),
                &schema_cols,
                Some(alias),
                Some(&agg_correlated),
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection =
            build_projection(&stmt.items, &schema_cols, alias, self.backslash_escapes)?;
        // v7.39 (round 621) — a target-list SRF expands here too. This tail
        // serves VALUES, a derived table and `ROWS FROM (…)`, and knew nothing
        // about them: `SELECT unnest(ARRAY[1,2]), x FROM (VALUES (3),(4)) v(x)`
        // answered `function unnest(integer[]) does not exist` for a query PG
        // answers.
        let srf_idxs = self.srf_target_idxs(&projection);
        let mut src_of_row: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        let mut projected_rows: alloc::vec::Vec<Row<'static>> =
            alloc::vec::Vec::with_capacity(filtered.len());
        if !srf_idxs.is_empty() {
            let (rows, src) =
                expand_projection_srfs(self, &projection, &srf_idxs, &filtered, &scan_ctx)?;
            projected_rows = rows;
            src_of_row = src;
        } else {
            for row in &filtered {
                let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                for p in &projection {
                    let v = self.eval_expr_with_correlated(
                        &p.expr,
                        row,
                        &scan_ctx,
                        cancel,
                        Some(&mut corr_memo.borrow_mut()),
                    )?;
                    vals.push(v);
                }
                projected_rows.push(Row::new(vals));
            }
        }
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            // v7.39 (read01 round 54) — keep the column's enum identity through
            // the projection (it lives outside the DataType lattice), or a
            // derived table / UNION / windowed result forgets it and any outer
            // `ORDER BY <enum col>` silently sorts by the label's TEXT.
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        // ORDER BY over the source rows (same shape as the other
        // synthetic-table executors).
        // v7.39 (read01 round 80) — a positional key (`ORDER BY 1`) means the Nth
        // OUTPUT column. Evaluated as an expression, as it was here, the literal
        // `1` is just the constant 1: the same sort key for every row, so the
        // sort ran and changed nothing. `SELECT unnest(ARRAY['B','a','A','b'])
        // ORDER BY 1` (which the parser turns into `SELECT * FROM unnest(…)`,
        // landing on this executor) came back in input order.
        let order_by = resolve_positional_order_by(&stmt.order_by, &projection);
        if !order_by.is_empty() {
            // v7.39 (round 621) — one entry per OUTPUT row, since a target-list
            // SRF makes more of them than there were inputs.
            let out_cols = if srf_idxs.is_empty() {
                alloc::vec![None; order_by.len()]
            } else {
                srf_order_output_cols(&order_by, &projection)
            };
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value<'static>>)> = projected_rows
                .iter()
                .enumerate()
                .map(|(k, out)| -> Result<_, EngineError> {
                    let r = &filtered[src_of_row.get(k).copied().unwrap_or(k)];
                    let keys: Result<Vec<Value<'static>>, EngineError> = order_by
                        .iter()
                        .zip(out_cols.iter())
                        .map(|(ob, oc)| {
                            // v7.39 (read01 round 54) — this path builds its
                            // sort keys itself instead of going through
                            // `build_order_keys`, so it skipped the enum-ordinal
                            // substitution: an OUTER `ORDER BY <enum col>` over
                            // a DERIVED TABLE sorted by the label TEXT, not by
                            // member order. Silently wrong rows, not an error.
                            let v = srf_order_key(ob, *oc, out, r, &scan_ctx)?;
                            Ok(
                                match crate::orderby::enum_order_ordinal(&ob.expr, &v, &scan_ctx) {
                                    Some(ord) => Value::Float(ord),
                                    None => v,
                                },
                            )
                        })
                        .collect();
                    Ok((k, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &stmt.order_by[idx];
                    let cmp = order_by_value_cmp_in(
                        o.desc,
                        o.nulls_first,
                        ka,
                        kb,
                        scan_ctx.mysql_dialect && !crate::eval::is_binary_coerced(&o.expr),
                    );
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        // v7.38 (read01) — DISTINCT over a synthetic source was dropped here.
        if stmt.distinct {
            projected_rows = dedup_rows(projected_rows, scan_ctx.mysql_dialect);
        }
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    /// Constant `SELECT` with no FROM: evaluate each projection item
    /// once against an empty dummy row (`SELECT 1`, `SELECT '7'::INT`).
    fn exec_constant_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        let empty_schema: Vec<ColumnSchema> = Vec::new();
        let ctx = self.ev_ctx(&empty_schema, None);
        // v7.39 (read01 round 106) — an aggregate with no FROM runs over the
        // single implicit row (`SELECT count(*)` → 1, `SELECT sum(5)` → 5,
        // `SELECT string_agg('x',',')` → x). Before this it fell through to the
        // scalar projection, where the aggregate name looked like an unknown
        // function. The WHERE filters that one row, so `… WHERE false` leaves
        // the aggregate zero input rows (`count(*)` → 0).
        if aggregate::uses_aggregate(stmt) {
            let dummy = Row::new(Vec::new());
            let passes = match &stmt.where_ {
                Some(w) => matches!(eval::eval_expr(w, &dummy, &ctx)?, Value::Bool(true)),
                None => true,
            };
            let rows: Vec<RowRef<'_>> = if passes {
                alloc::vec![RowRef::Owned(&dummy)]
            } else {
                Vec::new()
            };
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Refs(&rows),
                &empty_schema,
                None,
                None,
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, CancelToken::none());
        }
        let projection = build_projection(&stmt.items, &empty_schema, "", self.backslash_escapes)?;
        // `SELECT … WHERE cond` with no FROM — the one conceptual
        // row survives only when the condition is true (previously
        // the WHERE was silently ignored: `SELECT 1 WHERE false`
        // returned a row).
        let dummy_row = Row::new(Vec::new());
        if let Some(w) = &stmt.where_ {
            let cond = eval::eval_expr(w, &dummy_row, &ctx)?;
            if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                let columns: Vec<ColumnSchema> = projection
                    .into_iter()
                    .map(|p| {
                        let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                        c.user_enum_type = p.user_enum_type;
                        c.collation_name = p.collation_name;
                        c.mysql_fsp = p.mysql_fsp;
                        c
                    })
                    .collect();
                return Ok(QueryResult::Rows {
                    columns,
                    rows: Vec::new(),
                });
            }
        }
        // v7.38 (read01, T15) — a top-level SRF that the parser did NOT rewrite
        // into a FROM item (regexp_matches, whose rows are arrays and so cannot
        // desugar to unnest) expands here: one output row per SRF row, sibling
        // scalar columns repeated. unnest / array_elements / path_query reach a
        // real FROM via the parser rewrite and never land here.
        // v7.39 (read01 round 67) — every SRF in the list, in lockstep.
        let srf_idxs = self.srf_target_idxs(&projection);
        if !srf_idxs.is_empty() {
            let mut rows = expand_srf_row(self, &projection, &srf_idxs, &dummy_row, &ctx)?;
            let columns: Vec<ColumnSchema> = projection
                .into_iter()
                .map(|p| {
                    let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                    c.user_enum_type = p.user_enum_type;
                    c.collation_name = p.collation_name;
                    c.mysql_fsp = p.mysql_fsp;
                    c
                })
                .collect();
            // v7.39 (read01 round 80) — a FROM-less SELECT still has an ORDER BY,
            // an OFFSET and a LIMIT, and they apply to the rows the SRF expanded
            // to. This returned straight out of the expansion, so
            // `SELECT unnest(ARRAY['B','a','A','b']) ORDER BY 1` came back in
            // input order — the sort was not wrong, it never ran. (There is
            // exactly one conceptual input row here, which is why the ordinary
            // scan pipeline is not on this path at all.)
            if !stmt.order_by.is_empty() {
                let synth_ctx =
                    EvalContext::new(&columns, None).with_catalog(self.active_catalog());
                let resolved: Vec<spg_sql::ast::OrderBy> = stmt
                    .order_by
                    .iter()
                    .map(|o| {
                        let mut o = o.clone();
                        if let Expr::Literal(spg_sql::ast::Literal::Integer(n)) = &o.expr
                            && *n >= 1
                            && let Ok(idx) = usize::try_from(*n - 1)
                            && idx < columns.len()
                        {
                            o.expr = Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: columns[idx].name.clone(),
                            });
                        }
                        o
                    })
                    .collect();
                let descs: Vec<bool> = resolved.iter().map(|o| o.desc).collect();
                let mut tagged: Vec<(Vec<OrderKey>, Row)> = Vec::with_capacity(rows.len());
                for r in rows {
                    let keys = build_order_keys(&resolved, &r, &synth_ctx)?;
                    tagged.push((keys, r));
                }
                sort_by_keys(&mut tagged, &descs);
                rows = tagged.into_iter().map(|(_, r)| r).collect();
            }
            apply_offset_and_limit(&mut rows, stmt.offset_literal(), stmt.limit_literal());
            return Ok(QueryResult::Rows { columns, rows });
        }
        let mut values = Vec::with_capacity(projection.len());
        for p in &projection {
            values.push(eval::eval_expr(&p.expr, &dummy_row, &ctx)?);
        }
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type;
                c.collation_name = p.collation_name;
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        // v7.39 (round 239) — the FROM-less scalar path ignored LIMIT and
        // OFFSET entirely, so `SELECT 1 LIMIT 0` returned its row where PG
        // returns none. (The SRF and aggregate arms above already applied
        // them; this tail was the one that didn't.)
        let mut rows = alloc::vec![Row::new(values)];
        apply_offset_and_limit(&mut rows, stmt.offset_literal(), stmt.limit_literal());
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v7.37.x (docker-fair INSUBQ attack) — pre-replacement short-
    /// circuit. Catches
    ///   SELECT COUNT(*) FROM A WHERE A.pk IN (<uncorrelated subquery>)
    /// BEFORE `resolve_select_subqueries` materialises the inner result
    /// as `Vec<Expr::Literal>`. Runs the inner once, collects the
    /// values into a `HashSet<i64>` directly, then probes A.pk per
    /// HashSet entry and tallies. Saves the Expr-literal roundtrip
    /// (~150 µs / query at INSUBQ benchmark scale).
    pub(crate) fn try_count_star_pk_in_subquery_fast(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        use spg_sql::ast::SelectItem;
        if stmt.distinct
            || stmt.limit_with_ties
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.items.len() != 1
        {
            return Ok(None);
        }
        let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
            return Ok(None);
        };
        let is_count_star = matches!(expr, Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("count_star") && args.is_empty());
        if !is_count_star {
            return Ok(None);
        }
        let Some(from) = stmt.from.as_ref() else {
            return Ok(None);
        };
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.table_fn_call.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return Ok(None);
        }
        let Some(where_expr) = stmt.where_.as_ref() else {
            return Ok(None);
        };
        // The WHERE conjunct must be a bare `<col> IN (subquery)` with
        // negated=false; no other predicates.
        let Expr::InSubquery {
            expr: col_expr,
            subquery,
            negated: false,
        } = where_expr
        else {
            return Ok(None);
        };
        let Expr::Column(c) = col_expr.as_ref() else {
            return Ok(None);
        };
        let outer_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        if let Some(q) = c.qualifier.as_deref()
            && !q.eq_ignore_ascii_case(outer_alias)
        {
            return Ok(None);
        }
        // Outer column must be a single-column PK on integer family.
        let catalog = self.active_catalog();
        let Some(outer_table) = catalog.get(from.primary.name.as_str()) else {
            return Ok(None);
        };
        let outer_schema = outer_table.schema();
        let Some(outer_pos) = outer_schema
            .columns
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&c.name))
        else {
            return Ok(None);
        };
        if !matches!(
            outer_schema.columns[outer_pos].ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return Ok(None);
        }
        if !outer_schema
            .uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [outer_pos])
        {
            return Ok(None);
        }
        let Some(idx) = outer_table.index_on(outer_pos) else {
            return Ok(None);
        };
        // Inner must be uncorrelated. The cheap-correlation pre-check
        // exists upstream; here we just attempt the bare exec.
        if crate::subquery::select_is_correlated(subquery) {
            return Ok(None);
        }
        let mut inner = (**subquery).clone();
        self.resolve_select_subqueries(&mut inner, cancel)?;
        let r = match self.exec_bare_select_cancel(&inner, cancel) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let QueryResult::Rows { columns, rows, .. } = r else {
            return Ok(None);
        };
        if columns.len() != 1 {
            return Ok(None);
        }
        // v7.37.43 (INSUBQ B-1) — inner-uniqueness check. If the inner
        // subquery projects a column known to be UNIQUE/PK on its table
        // (statically: `SELECT <col> FROM <tbl> WHERE …` where <col> is
        // in `tbl.uniqueness_constraints`), survivor values are
        // guaranteed distinct and the per-survivor `HashSet::insert`
        // dedup check is redundant. ~25 ns × N_inner-survivors saved.
        //
        // Inlined check — gated on: no DISTINCT/GROUP/UNION/JOIN, single
        // projection that is a bare Column ref, table-column lookup in
        // catalog confirms the column appears as a unique constraint's
        // sole member. UNIQUE NOT NULL is required — a nullable unique
        // column may have multiple NULLs, but NULLs are already skipped
        // above (`Value::Null => continue`), so a UNIQUE-only column is
        // still safe to dedup-skip.
        let inner_unique = (|| -> bool {
            if inner.distinct
                || inner.group_by.is_some()
                || !inner.unions.is_empty()
                || inner.having.is_some()
                || inner.items.len() != 1
            {
                return false;
            }
            let Some(inner_from) = inner.from.as_ref() else {
                return false;
            };
            if !inner_from.joins.is_empty()
                || inner_from.primary.lateral_subquery.is_some()
                || inner_from.primary.unnest_expr.is_some()
                || inner_from.primary.generate_series_args.is_some()
                || inner_from.primary.table_fn_call.is_some()
            {
                return false;
            }
            let SelectItem::Expr { expr: proj, .. } = &inner.items[0] else {
                return false;
            };
            let Expr::Column(pc) = proj else {
                return false;
            };
            let inner_alias = inner_from
                .primary
                .alias
                .as_deref()
                .unwrap_or(inner_from.primary.name.as_str());
            if let Some(q) = pc.qualifier.as_deref()
                && !q.eq_ignore_ascii_case(inner_alias)
            {
                return false;
            }
            let Some(inner_table) = catalog.get(inner_from.primary.name.as_str()) else {
                return false;
            };
            let isch = inner_table.schema();
            let Some(ipos) = isch
                .columns
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(&pc.name))
            else {
                return false;
            };
            isch.uniqueness_constraints
                .iter()
                .any(|u| u.columns.as_slice() == [ipos])
        })();
        // Collect inner i64 values directly into a HashSet, then probe.
        let mut count: i64 = 0;
        let mut probed = if inner_unique {
            hashbrown::HashSet::<i64>::new()
        } else {
            hashbrown::HashSet::<i64>::with_capacity(rows.len())
        };
        for row in &rows {
            let v = row.values.first().cloned().unwrap_or(Value::Null);
            let n = match v {
                Value::BigInt(n) => n,
                Value::Int(n) => i64::from(n),
                Value::SmallInt(n) => i64::from(n),
                Value::Null => continue,
                _ => return Ok(None),
            };
            // De-duplicate inner key set so a duplicate inner value
            // doesn't double-count the same outer row. Skipped when
            // the inner projection is statically unique.
            if !inner_unique && !probed.insert(n) {
                continue;
            }
            // v7.37.43 (INSUBQ B-2 + B-4) — direct i64 PK probe, skipping
            // the `IndexKey::from_value` enum-dispatch and the per-call
            // `IndexKey` wrapper construction. The outer column is
            // already gated to integer-family above, so an i64 key
            // always corresponds to a valid PK lookup.
            if !idx.lookup_eq_i64(n).is_empty() {
                count += 1;
            }
        }
        let columns_out = alloc::vec![ColumnSchema::new(
            "count".to_string(),
            spg_storage::DataType::BigInt,
            false,
        )];
        let rows_out = alloc::vec![Row::new(alloc::vec![Value::BigInt(count)])];
        Ok(Some(QueryResult::Rows {
            columns: columns_out,
            rows: rows_out,
        }))
    }

    /// v7.37.x (docker-fair INSUBQ attack) — short-circuit
    ///   SELECT COUNT(*) FROM A WHERE A.pk IN (literal list)
    /// (the post-subquery-replacement shape of the INSUBQ probe
    /// `SELECT COUNT(*) FROM A WHERE A.pk IN (SELECT k FROM B WHERE …)`).
    /// The general aggregate path materialises every seeked row into
    /// a `Vec<Cow<Row>>`, then runs the aggregate executor over it.
    /// For COUNT(*) we only care how many keys hit; iterate the list
    /// and tally `idx.lookup_eq(key)` non-empty results, skipping the
    /// row materialisation, the aggregate state machine, and the per-
    /// row WHERE re-eval (the seek already filtered by the same list).
    /// Returns `None` when the shape doesn't match.
    fn try_count_star_pk_in_list_fast(
        &self,
        stmt: &SelectStatement,
        table: &spg_storage::Table,
        schema_cols: &[ColumnSchema],
        alias: &str,
    ) -> Option<QueryResult> {
        use spg_sql::ast::{ColumnName, SelectItem};
        // Gates on the SELECT shape.
        if stmt.distinct
            || stmt.limit_with_ties
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.items.len() != 1
        {
            return None;
        }
        let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
            return None;
        };
        let is_count_star = matches!(expr, Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("count_star") && args.is_empty());
        if !is_count_star {
            return None;
        }
        // WHERE must be `<col> IN (literal list)` with no other
        // conjuncts (the seek result is a true subset of the row
        // population for this predicate).
        let where_expr = stmt.where_.as_ref()?;
        let Expr::InList {
            expr: col_expr,
            list,
            negated: false,
        } = where_expr
        else {
            return None;
        };
        let Expr::Column(c) = col_expr.as_ref() else {
            return None;
        };
        if let Some(q) = c.qualifier.as_deref()
            && !q.eq_ignore_ascii_case(alias)
        {
            return None;
        }
        let col_pos = schema_cols
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&c.name))?;
        // The column must be a single-column PK on an integer family
        // — the same gate the SCALARSQ + LEFT-ANTI-JOIN fast paths use,
        // so the antiset stays collision-free under `HashSet<i64>`.
        let schema = table.schema();
        if !matches!(
            schema.columns[col_pos].ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return None;
        }
        if !schema
            .uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [col_pos])
        {
            return None;
        }
        let idx = table.index_on(col_pos)?;
        // Tally non-empty seek results across all literal values.
        let mut count: i64 = 0;
        for lit in list {
            let Expr::Literal(l) = lit else {
                return None;
            };
            let v = eval::literal_to_value(l);
            let key = spg_storage::IndexKey::from_value(&v)?;
            if !idx.lookup_eq(&key).is_empty() {
                count += 1;
            }
        }
        let columns = alloc::vec![ColumnSchema::new(
            "count".to_string(),
            spg_storage::DataType::BigInt,
            false,
        )];
        let rows = alloc::vec![Row::new(alloc::vec![Value::BigInt(count)])];
        let _ = ColumnName {
            qualifier: None,
            name: String::new(),
        };
        Some(QueryResult::Rows { columns, rows })
    }

    /// v7.38 (perf, exact-range count) — `SELECT count(*) FROM t WHERE <col>
    /// BETWEEN a AND b` on an indexed column. The index range walk yields
    /// exactly the matching (visible) rows, so we count locators directly —
    /// skipping the row materialisation, the aggregate state machine, and the
    /// per-row WHERE re-eval the general path pays. Turns the `range_count`
    /// endpoint from tied-with-PG (superset re-eval) into a clear win. None
    /// when the shape doesn't match.
    fn try_count_star_indexed_range_fast(
        &self,
        stmt: &SelectStatement,
        table: &spg_storage::Table,
        schema_cols: &[ColumnSchema],
        alias: &str,
        snapshot: &spg_storage::snapshot::Snapshot,
    ) -> Option<QueryResult> {
        use spg_sql::ast::SelectItem;
        if stmt.distinct
            || stmt.limit_with_ties
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.items.len() != 1
        {
            return None;
        }
        let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
            return None;
        };
        let is_count_star = matches!(expr, Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("count_star") && args.is_empty());
        if !is_count_star {
            return None;
        }
        let where_expr = stmt.where_.as_ref()?;
        let count =
            crate::index_access::try_range_count(where_expr, schema_cols, table, alias, snapshot)?;
        let columns = alloc::vec![ColumnSchema::new(
            "count".to_string(),
            spg_storage::DataType::BigInt,
            false,
        )];
        let rows = alloc::vec![Row::new(alloc::vec![Value::BigInt(count)])];
        Some(QueryResult::Rows { columns, rows })
    }

    /// Single-table aggregate path: filter the (optionally index-seeked)
    /// rows, then hand off to the aggregate executor which does its own
    /// projection + ORDER BY before `finish_agg_result` applies LIMIT.
    fn run_single_table_aggregate<'a>(
        &self,
        stmt: &SelectStatement,
        table: &'a spg_storage::Table,
        schema_cols: &'a [ColumnSchema],
        alias: &str,
        indexed_rows: Option<Vec<Cow<'a, Row<'static>>>>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.38 (read01 U15) — per-scan sampler cell for TABLESAMPLE
        // REPEATABLE (see run_single_table_scan). Aggregates
        // (`count(*) FROM t TABLESAMPLE …`) filter through this ctx too.
        let sample_cell: core::cell::Cell<Option<u64>> = core::cell::Cell::new(None);
        let ctx = self
            .ev_ctx(schema_cols, Some(alias))
            .with_sample_rng(&sample_cell);
        // v7.39 (round 657) — pre-sized. Pushing 500k pointers into a
        // `Vec::new()` walks the doubling chain 8, 16, … 262144, 524288,
        // and every abandoned buffer on the way stays resident: RSS is a
        // high-water mark, so the intermediates are paid for even though
        // they are freed. Round 656 measured the scan at 17 bytes/row
        // where the survivor list itself only needs 8.
        let mut filtered: Vec<&Row<'static>> = if stmt.where_.is_none() {
            Vec::with_capacity(table.rows().len())
        } else {
            // With a WHERE, the row count is an UPPER bound and reserving it
            // is the worse trade: `… WHERE id = 5` over 50M rows would take
            // 400 MB of pointers to hold one survivor. Let it grow.
            Vec::new()
        };
        // v6.2.6 — Memoize: per-query LRU cache for correlated
        // scalar subqueries. Fresh per row-loop entry so each
        // SELECT execution gets an isolated cache.
        let mut memo = memoize::MemoizeCache::new();
        // v7.37 (perf) — single-table aggregate's WHERE filter
        // pre-7.37 ran the slow tree-walker (`eval_expr_with_
        // correlated`) per row, even for subquery-free WHEREs that
        // the single-table SCAN path has compiled since v7.32
        // (perf knife D). The asymmetry meant a fold-to-filter
        // rewrite (joinfold) that swapped a JOIN for a single-table
        // aggregate over a compiled WHERE saw the tree-walker
        // instead — 25 k rows × `m.mailbox_id IN (25 lits)` cost
        // ~9 ms via the walker, vs ~1 ms via the compiled InSet
        // step. Compile once if eligible; fall back to the walker
        // for subquery-bearing or non-compilable WHEREs.
        let compiled_where: Option<eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| eval::fully_compilable(w))
            .map(|w| eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        let mut row_passes_where = |row: &Row<'static>,
                                    eval_stack: &mut Vec<Value<'static>>,
                                    memo: &mut memoize::MemoizeCache|
         -> Result<bool, EngineError> {
            match (&compiled_where, &stmt.where_) {
                (Some(cw), _) => {
                    // v7.39 (round 479) — the predicate wants a bool, not a
                    // Value. The owned entry ended in `Value::into_owned`
                    // and the caller then dropped it, once per row; round
                    // 478's profile put that pair above the comparison
                    // itself.
                    Ok(eval::compiled::eval_compiled_pred(
                        cw,
                        row,
                        &ctx,
                        eval_stack,
                        ctx.mysql_dialect,
                    )
                    .map_err(EngineError::Eval)?)
                }
                (None, Some(w)) => {
                    let cond = self.eval_expr_with_correlated(w, row, &ctx, cancel, Some(memo))?;
                    Ok(crate::eval::predicate_is_true(
                        &cond,
                        "WHERE",
                        ctx.mysql_dialect,
                    )?)
                }
                (None, None) => Ok(true),
            }
        };
        if let Some(rows) = &indexed_rows {
            for cow in rows {
                let row = cow.as_ref();
                if !row_passes_where(row, &mut eval_stack, &mut memo)? {
                    continue;
                }
                filtered.push(row);
            }
        }
        // v7.36 (cold-tier coverage) — single-table aggregate's
        // non-indexed full scan was hot-only and silently lost cold
        // rows on COUNT/SUM/etc. Materialise cold rows once into
        // `cold_rows_storage` (Vec<Row<'static>>) so the `filtered: Vec<&Row<'static>>`
        // shape stays unchanged; the cold rows live until the end of
        // the aggregate run.
        let cold_rows_storage = if indexed_rows.is_none() {
            self.iter_cold_rows_of_table(table)
        } else {
            Vec::new()
        };
        if indexed_rows.is_none() {
            // v7.37.15 (Phase C.3, step 2) — MVCC visibility gate for the
            // single-table aggregate full-scan path. Mirrors the gate on
            // `run_single_table_scan`: this is a user-query result path,
            // so under gate-on (`SPG_MVCC_INPLACE`) it must skip rows the
            // reader's snapshot cannot see (e.g. tombstoned versions),
            // otherwise COUNT/SUM/etc. would tally dead rows. A no-op
            // under the default gate-off: every hot row is frozen or
            // committed-and-alive, so `is_row_visible` returns true.
            // Cold-tier rows are frozen (visible) by definition — left
            // ungated, matching the plain-scan path.
            let scan_snapshot = self.current_snapshot();
            // v7.39 (pg_stat knife B) — this full-scan branch walks
            // headers directly (serial and sharded alike); count the
            // sequential scan here.
            table.note_seq_scan();
            // v7.39 (parallel-agg P2) — the visibility probe + WHERE
            // filter dominate the pre-aggregate wall time on big
            // scans (P1's ground truth: accumulation is only ~17%).
            // Shard THAT work when the host injected an executor and
            // the WHERE is compiled (the compiled evaluator is pure
            // over &row; the tree-walker fallback can hit correlated
            // subqueries and stays serial). Shards return surviving
            // ROW INDICES — &Row can't cross the Box<dyn Any>'s
            // 'static bound — and the main thread only dereferences.
            let n = table.row_count();
            let par = self.parallel_runner.0.as_deref().filter(|_| {
                n >= crate::PARALLEL_MIN_ROWS && (stmt.where_.is_none() || compiled_where.is_some())
            });
            if let Some(r) = par {
                let n_shards = (n / crate::PARALLEL_MIN_ROWS).clamp(2, 8);
                let chunk = n.div_ceil(n_shards);
                type ShardOut = Result<alloc::vec::Vec<usize>, EngineError>;
                let cw = &compiled_where;
                let snap_ref = &scan_snapshot;
                let results = r.run_shards(n_shards, &|s| {
                    let lo = s * chunk;
                    let hi = ((s + 1) * chunk).min(n);
                    let mut keep: alloc::vec::Vec<usize> = alloc::vec::Vec::with_capacity(hi - lo);
                    // EvalContext carries Cells (sampler / row counters)
                    // and is !Sync — each shard builds its own from the
                    // same Sync inputs. The compiled WHERE is gated to
                    // the pure-scalar whitelist, which reads none of the
                    // session state the engine-built ctx would add
                    // (TABLESAMPLE's __tsm_fract is not whitelisted, so
                    // sampled scans never take this branch).
                    let shard_ctx = EvalContext::new(schema_cols, Some(alias));
                    let mut stack: Vec<Value<'static>> = Vec::new();
                    let out: ShardOut = (|| {
                        for i in lo..hi {
                            if !table.is_row_visible(i, snap_ref) {
                                continue;
                            }
                            let row = &table.rows()[i];
                            // v7.39 (round 480) — the parallel full-scan
                            // shard is the path the aggregate benchmark
                            // actually takes, and it was still on the OWNED
                            // entry: round 480's profile attributed 68.7 %
                            // of `drop_glue<Value>` to this closure, which
                            // is why round 479's fix to the indexed path
                            // barely moved the total.
                            //
                            // The `matches!(…, Value::Bool(true))` form was
                            // also a narrower reading than the rest of the
                            // engine uses — `predicate_is_true` is what
                            // handles NULL and MySQL truthiness — so the
                            // bool entry fixes the shape as well as the cost.
                            let pass = match cw {
                                Some(c) => eval::compiled::eval_compiled_pred(
                                    c,
                                    row,
                                    &shard_ctx,
                                    &mut stack,
                                    shard_ctx.mysql_dialect,
                                )
                                .map_err(EngineError::Eval)?,
                                None => true,
                            };
                            if pass {
                                keep.push(i);
                            }
                        }
                        Ok(keep)
                    })();
                    alloc::boxed::Box::new(out)
                });
                // v7.39 (round 567) — `rows()` is a 32-way trie, so
                // indexing it is four dependent loads and a scan that
                // reads every row paid them every row. A profile of
                // `SELECT sum(id)` over 500k rows put 37.8% of the
                // connection thread's CPU on THIS ONE LINE. The cursor
                // holds the leaf, making that one descent per 32.
                let mut rows_cur = table.rows().run_cursor();
                for boxed in results {
                    let shard = boxed
                        .downcast::<ShardOut>()
                        .expect("runner echoes the closure's box");
                    for i in (*shard)? {
                        if let Some(row) = rows_cur.get(i) {
                            filtered.push(row);
                        }
                    }
                }
            } else {
                let mut rows_cur = table.rows().run_cursor();
                for i in 0..n {
                    if !table.is_row_visible(i, &scan_snapshot) {
                        continue;
                    }
                    let Some(row) = rows_cur.get(i) else { continue };
                    if !row_passes_where(row, &mut eval_stack, &mut memo)? {
                        continue;
                    }
                    filtered.push(row);
                }
            }
            for row in &cold_rows_storage {
                if !row_passes_where(row, &mut eval_stack, &mut memo)? {
                    continue;
                }
                filtered.push(row);
            }
        }
        // v7.29 — a per-query memo so correlated scalar
        // subqueries batch-evaluate once (group map) instead of
        // executing per group.
        let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
        let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
            self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                .map_err(|err| match err {
                    EngineError::Eval(ev) => ev,
                    other => eval::EvalError::TypeMismatch {
                        detail: alloc::format!("{other}"),
                    },
                })
        };
        // v7.39 (round 656) — the plain relational scan. This collect() was
        // the measured defect: one 64-byte `RowRef` per surviving row to
        // wrap an 8-byte pointer `filtered` already holds. Scalar
        // aggregates measured ~81 bytes/row of working memory because of
        // it — 40 MB at 500k rows, 3.2 GB at 50M, for a query that returns
        // one number. `AggRows::Ptrs` reads the pointers directly.
        let agg = aggregate::run(
            stmt,
            crate::join::AggRows::Ptrs(&filtered),
            schema_cols,
            Some(alias),
            Some(&agg_correlated),
            self.parallel_runner.0.as_deref(),
            Some(self.active_catalog()),
            Some(self),
        )?;
        self.finish_agg_result(agg, stmt, cancel)
    }

    /// Single-table scan + projection path: WHERE filter (compiled when
    /// subquery-free), ORDER BY keying, SRF expansion / projection, then
    /// sort + WITH TIES / DISTINCT / OFFSET-LIMIT.
    fn run_single_table_scan<'a>(
        &self,
        stmt: &SelectStatement,
        table: &'a spg_storage::Table,
        schema_cols: &'a [ColumnSchema],
        alias: &str,
        indexed_rows: Option<Vec<Cow<'a, Row<'static>>>>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.38 (read01 U15) — a fresh per-scan sampler cell for
        // `TABLESAMPLE … REPEATABLE(seed)`. Created before the ctx so the
        // deterministic `__tsm_fract(seed)` draws share one scan-local
        // state (isolated from the global random() PRNG); a fresh cell per
        // scan makes a repeat / rescan reproduce the same sample. Unused
        // and cheap when the query carries no sample.
        let sample_cell: core::cell::Cell<Option<u64>> = core::cell::Cell::new(None);
        let ctx = self
            .ev_ctx(schema_cols, Some(alias))
            .with_sample_rng(&sample_cell);
        let projection = build_projection(&stmt.items, schema_cols, alias, self.backslash_escapes)?;
        // v7.19 P5 — single-table SELECT path for SRF
        // `SELECT unnest(arr) FROM t` shape. Detect a top-level
        // unnest in the projection list. When present, the
        // per-row processor emits one output row per array
        // element (broadcasting non-SRF projections from the
        // same input row). Empty / NULL arrays emit zero rows
        // for that input — PG semantics.
        // v7.39 (read01 round 67) — every SRF in the target list, in lockstep.
        let srf_idxs = self.srf_target_idxs(&projection);
        let srf_position = srf_idxs.first().copied();
        // v7.39 (round 599) — the SRF analysis is per QUERY, not per row.
        let mut srf_plan = if srf_position.is_some() {
            Some(build_srf_plan(self, &projection, &srf_idxs, &ctx)?)
        } else {
            None
        };

        // Materialise the filter pass into `(order_key, projected_row)`
        // tuples. The order key is `None` when there's no ORDER BY clause.
        let mut tagged: Vec<(Vec<OrderKey>, Row<'static>)> = Vec::new();
        // v7.33 (C1, ceiling-first/never-die) — charge each accumulated
        // output row to the per-query byte budget as it is built, so a
        // fat single-table scan / sort REJECTS with QueryBytesExceeded
        // at ~the ceiling instead of materialising the whole table and
        // only noticing at the final enforce_row_limit check. Without
        // this, N concurrent fat scans peak at N×table and OOM the host.
        // `max_query_bytes = None` (the embedded default) = no ceiling,
        // so existing unbudgeted behaviour is byte-identical.
        let mut budget = ByteBudget::new(self.max_query_bytes);
        // v6.2.6 — Memoize per-row WHERE eval shares one cache.
        let mut memo = memoize::MemoizeCache::new();
        // v7.32 (perf knife D) — subquery-free WHERE compiles once;
        // the row loop then runs a flat step program instead of a
        // tree interpretation per row.
        let compiled_where: Option<eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| eval::fully_compilable(w))
            .map(|w| eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        // v7.37.x (docker-fair SCALARSQ attack) — pre-analyse every
        // SELECT-item scalar subquery for the PK-probe fast path. The
        // analysis (gate checks + catalog lookups) takes ~500 ns; doing
        // it once per query instead of once per row × 100 rows saves
        // ~50 µs and lets the per-row evaluation reduce to a single
        // index probe + outer-column read.
        let scalarsq_fast: Vec<Option<crate::ScalarPkProbeFastPath>> = projection
            .iter()
            .map(|p| {
                if let Expr::ScalarSubquery(inner) = &p.expr {
                    self.analyse_scalar_count_pk_eq_probe(inner, schema_cols, alias)
                } else {
                    None
                }
            })
            .collect();
        let any_scalarsq_fast = scalarsq_fast.iter().any(Option::is_some);
        // v7.39 (round 487) — a projection item that is a bare column
        // reference binds its position ONCE per query.
        //
        // Per row it used to walk `eval_expr_with_correlated` (a memo
        // lookup for "does this have a subquery", then an un-memoised
        // `expr_may_use_in_set` tree walk), then `eval_expr`'s dispatch,
        // then `resolve_column`, which finds the column by scanning the
        // schema and comparing NAMES. On `SELECT g FROM h` that chain was
        // 19 % of self time for what is ultimately one cell read.
        //
        // `compile_column_pos` is the Step VM's resolver, already
        // `pub(crate)` and already reused by the aggregate's bind-once
        // path: it mirrors `resolve_column`'s happy layers and returns
        // None for anything that would reach an error, an ambiguity, or a
        // miss, so those still go the interpreter's way and keep its
        // exact message. A composite column is excluded for the same
        // reason `compile_into` excludes it — it must be rehydrated from
        // stored JSON, which is not a cell read.
        let proj_direct = bind_direct_columns(&projection, &ctx);
        let any_proj_direct = proj_direct.iter().any(Option::is_some);
        // v7.39 (round 605) — a projection item that cannot depend on the row
        // is evaluated once. `SELECT ('{"a":1}')::JSONB FROM j` cost TEN
        // allocations a row against one for a plain column, `'abc' || 'def'`
        // six and `upper('abc')` five, all of them producing the same value
        // 50,000 times. An item that fails to evaluate is left alone, so its
        // error still comes from the row loop in the interpreter's wording.
        let proj_const: Vec<Option<Value<'static>>> = projection
            .iter()
            .map(|p| crate::eval::compiled::constant_projection_value(&p.expr, &ctx))
            .collect();
        let any_proj_const = proj_const.iter().any(Option::is_some);
        crate::bump_counter!(crate::select::SCAN_PATH_ENTERED);
        // v7.39 (read01 round 80) — positional ORDER BY over a WILDCARD
        // projection. Statement prep (`resolve_order_by_position`) can only map
        // `ORDER BY 1` onto the first SELECT item when that item is an
        // expression; a `*` is not one, so the literal survived to here and was
        // evaluated as the CONSTANT 1 — the same key for every row, i.e. no sort
        // at all. The parser rewrites `SELECT unnest(a) x` into
        // `SELECT * FROM unnest(a) x`, so that innocuous-looking shape landed
        // exactly here: `SELECT unnest(ARRAY['B','a','A','b']) ORDER BY 1` came
        // back in input order. The projection is built by now, so the Nth output
        // column is known — resolve against it.
        let order_by = resolve_positional_order_by(&stmt.order_by, &projection);
        // v7.39 (round 600) — the ORDER BY of an SRF query is decided on the
        // EXPANDED rows, so a key naming a select-list item reads that item.
        let srf_order_cols: Vec<Option<usize>> = if srf_position.is_some() {
            srf_order_output_cols(&order_by, &projection)
        } else {
            Vec::new()
        };
        let srf_key_bound: Vec<Option<usize>> = (0..order_by.len()).map(Some).collect();
        // v7.37.x (docker-fair SCALARSQ attack) — early-limit gate for
        // the no-ORDER-BY-no-DISTINCT-no-TIES-no-SRF-no-WHERE shape.
        // Hoisted above the closure so the projection-eval path can
        // gate `memo` passing on it: the SELECT-item correlated-scalar
        // batch path scans the FULL inner table once (~5 ms for 12.5 k
        // rows) and is only a win when N outer rows is large; for small
        // LIMITed shapes a per-row PK seek (~5 µs × 100 = 500 µs) wins.
        let early_cap: Option<usize> = if order_by.is_empty()
            && !stmt.distinct
            && !stmt.limit_with_ties
            && srf_position.is_none()
            && stmt.where_.is_none()
        {
            stmt.limit_literal()
                .map(|n| n.saturating_add(stmt.offset_literal().unwrap_or(0)) as usize)
        } else {
            None
        };
        // v7.38 (read01 B8) — streaming top-N budget. For `ORDER BY …
        // LIMIT k` (no DISTINCT / WITH TIES / SRF, and not forced to
        // full-sort by the test gate) keep only the running top-`keep`
        // rows in memory instead of materialising every projected row,
        // so a `… ORDER BY col LIMIT 10` over a huge table is O(keep)
        // space, not O(rows). `None` = accumulate everything (the prior
        // behaviour). The final `partial_sort_tagged(keep)` below still
        // runs and produces the identical rows.
        // v7.39 (round 683) — the declared collation for each ORDER BY
        // position, resolved once and carried beside `descs` for the same
        // reason `descs` is carried: it is per key position, not per row.
        let order_colls = crate::orderby::order_by_collations(&order_by, &ctx)?;
        let topk_stream: Option<(usize, Vec<bool>)> = if !order_by.is_empty()
            && !stmt.distinct
            && !stmt.limit_with_ties
            && srf_position.is_none()
            && !self.env_cfg().disable_topk
        {
            stmt.limit_literal().and_then(|l| {
                let keep = (l as usize).saturating_add(stmt.offset_literal().unwrap_or(0) as usize);
                (keep >= 1).then(|| (keep, order_by.iter().map(|o| o.desc).collect()))
            })
        } else {
            None
        };
        // v7.37.16 — streaming DISTINCT seen-set: norm-hash → indices of
        // kept rows in `tagged`. Probing on the PROJECTED row as soon as
        // it is built means a duplicate costs neither a build_order_keys
        // eval (the dominant per-row cost of `DISTINCT … ORDER BY`) nor
        // a tagged slot, and the sort below runs over u survivors, not
        // n input rows — PG's hash-distinct-then-sort plan shape.
        let mut seen_distinct: hashbrown::HashMap<u64, crate::distinct::DistinctBucket> =
            hashbrown::HashMap::new();
        let distinct_hb = hashbrown::DefaultHashBuilder::default();
        // v7.39 (round 485) — one projection buffer for the whole scan
        // rather than a fresh `Vec` per input row. A row that survives
        // the DISTINCT probe takes the buffer with it (`mem::take`) and
        // the next row allocates a new one; a row that duplicates an
        // earlier one leaves the buffer — and its capacity — in place.
        // The round-485 counter says 49 900 of `distinct_proj`'s 50 000
        // projected rows are duplicates, so that is 49 900 allocate /
        // free pairs the scan no longer performs. Shapes where every row
        // survives (plain projection, `DISTINCT` over a unique column)
        // allocate exactly as often as before.
        let mut proj_buf: Vec<Value<'static>> = Vec::new();
        // v7.39 (round 571) — buffers handed back by the top-N trim.
        // Round 485 made the scan share ONE projection buffer, but a
        // surviving row takes it (`mem::take`) and without DISTINCT
        // almost every row survives, so the next one starts from zero
        // capacity and allocates. The trim drops `keep` rows at a time
        // and their buffers come back here instead of being freed.
        let mut proj_pool: Vec<Vec<Value<'static>>> = Vec::new();
        let mut key_pool: Vec<Vec<crate::orderby::OrderKey>> = Vec::new();
        // v7.39 (round 581) — the worst row the accumulator is currently
        // keeping. Anything that loses to it cannot reach the answer, so
        // it is dropped before its projection is ever built.
        let mut topk_boundary: Option<Vec<crate::orderby::OrderKey>> = None;
        // v7.39 (round 582) — resolve each ORDER BY column once, not
        // once per row. See `order_by_bound_positions`.
        let order_bound =
            crate::orderby::order_by_bound_positions(&order_by, schema_cols, Some(alias));
        // v7.39 (round 581) — and it stops asking when the answer is
        // always "keep".
        //
        // The check earns its place only on rows it rejects. Over
        // ascending ids, `ORDER BY id DESC` never rejects one — every
        // row beats the current worst — so the comparison is pure
        // overhead there, measured at +5.5% in three batches out of
        // three. After a window of rows it looks at what it has
        // actually rejected and switches itself off if the shape is not
        // paying. The answers do not depend on it either way.
        const BOUNDARY_WINDOW: u32 = 8192;
        let mut boundary_checks: u32 = 0;
        let mut boundary_rejects: u32 = 0;
        let mut boundary_check_on = true;
        // Inline the per-row work in a closure so the indexed and full-
        // scan branches share the body.
        let mut process_row = |row: &Row<'static>, loop_idx: usize| -> Result<(), EngineError> {
            if loop_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(cw) = &compiled_where {
                let cond = eval::eval_compiled(cw, row, &ctx, &mut eval_stack)
                    .map_err(EngineError::Eval)?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    return Ok(());
                }
            } else if let Some(where_expr) = &stmt.where_ {
                let cond =
                    self.eval_expr_with_correlated(where_expr, row, &ctx, cancel, Some(&mut memo))?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    return Ok(());
                }
            }
            // Under DISTINCT the keys are built AFTER the dup probe
            // (survivors only); the non-distinct order is unchanged.
            // v7.39 (round 600) — an SRF query's keys are built per EXPANDED
            // row further down, and building them here would evaluate the
            // ORDER BY against the INPUT row: a key naming the SRF's own
            // output became a scalar call to it, which is where
            // "function unnest(integer[]) does not exist" came from.
            let order_keys = if order_by.is_empty() || stmt.distinct || srf_position.is_some() {
                Vec::new()
            } else {
                let mut buf = key_pool.pop().unwrap_or_default();
                crate::orderby::build_order_keys_bound(
                    &order_by,
                    &order_bound,
                    row,
                    &ctx,
                    &mut buf,
                )?;
                // v7.39 (round 581) — reject before projecting.
                //
                // `ORDER BY g DESC, id DESC LIMIT 10` over 500k rows with
                // 50 distinct `g` decides nearly every row on the FIRST
                // key, and PG answers it FASTER than the single-key form
                // (7.4 ms against 10.4) because a rejected row costs it
                // one comparison. SPG built both keys AND the projected
                // row for all 500k before throwing them away. The keys
                // are needed to compare; the projection is not.
                if boundary_check_on
                    && let Some((_, descs)) = &topk_stream
                    && let Some(b) = &topk_boundary
                {
                    boundary_checks += 1;
                    let loses = crate::orderby::cmp_multi_key_in(&buf, b, descs, &order_colls)
                        == core::cmp::Ordering::Greater;
                    if loses {
                        boundary_rejects += 1;
                    }
                    if boundary_checks == BOUNDARY_WINDOW {
                        // Keep asking only if it has been rejecting at
                        // least a quarter of what it saw.
                        boundary_check_on = boundary_rejects.saturating_mul(4) >= boundary_checks;
                    }
                    if loses {
                        buf.clear();
                        key_pool.push(buf);
                        return Ok(());
                    }
                }
                buf
            };
            if srf_position.is_some() {
                let plan = srf_plan.as_mut().expect("srf_position implies a plan");
                for out in expand_srf_row_with(self, plan, &projection, row, &ctx)? {
                    if stmt.distinct {
                        let bucket = seen_distinct
                            .entry(norm_hash_row(&out, &distinct_hb, ctx.mysql_dialect))
                            .or_default();
                        if bucket
                            .iter()
                            .any(|i| row_eq_norm(&tagged[i].1, &out, ctx.mysql_dialect))
                        {
                            continue;
                        }
                        bucket.push(tagged.len());
                    }
                    budget.charge(approx_row_bytes(&out))?;
                    // The keys come from THIS expanded row: a key naming a
                    // select-list item reads its value, anything else is
                    // still evaluated against the input row.
                    let keys = if order_by.is_empty() {
                        Vec::new()
                    } else {
                        let mut kv: Vec<Value<'static>> = Vec::with_capacity(order_by.len());
                        for (k, ob) in order_by.iter().enumerate() {
                            kv.push(match srf_order_cols.get(k).copied().flatten() {
                                Some(p) => out.values.get(p).cloned().unwrap_or(Value::Null),
                                None => eval::eval_expr(&ob.expr, row, &ctx)
                                    .map_err(EngineError::Eval)?,
                            });
                        }
                        // Packed by the same code every other ORDER BY uses,
                        // so DESC / NULLS FIRST / the MySQL rule are not
                        // restated here.
                        let key_row = Row::new(kv);
                        let mut buf = Vec::new();
                        crate::orderby::build_order_keys_bound(
                            &order_by,
                            &srf_key_bound,
                            &key_row,
                            &ctx,
                            &mut buf,
                        )?;
                        buf
                    };
                    tagged.push((keys, out));
                }
            } else {
                let values = &mut proj_buf;
                values.clear();
                values.reserve(projection.len());
                for (i, p) in projection.iter().enumerate() {
                    // v7.37.x (docker-fair SCALARSQ attack) — pre-
                    // analysed PK-probe fast path. The per-row work is
                    // a read of outer.col from the row plus an index
                    // probe — no Expr clone, no walker, no
                    // `eval_expr_with_correlated` framework.
                    if any_scalarsq_fast && let Some(fp) = &scalarsq_fast[i] {
                        values.push(self.probe_with_pk_fast_path(fp, row));
                        continue;
                    }
                    // v7.39 (round 605) — the same value every row.
                    if any_proj_const && let Some(v) = &proj_const[i] {
                        values.push(v.clone());
                        continue;
                    }
                    // v7.39 (round 487) — bound column: read the cell.
                    // This is `rehydrate_cell`'s body for a non-composite
                    // column, which is what the whole chain below reduces
                    // to once the name has been resolved.
                    if any_proj_direct && let Some(pos) = proj_direct[i] {
                        crate::bump_counter!(crate::select::PROJ_DIRECT_FIRE);
                        values.push(row.values[pos].clone().into_owned());
                        continue;
                    }
                    // v7.24 (round-16 B) — correlated-aware.
                    // v7.37.x (docker-fair SCALARSQ attack) — share the
                    // per-row memo with projection. Required for the
                    // batch-evaluated correlated-scalar path to fire on
                    // SELECT-item scalar subqueries; otherwise each row
                    // re-executes the inner.
                    //
                    // Skip the memo when the outer row count is small
                    // (early-limited): the batch path scans the FULL
                    // inner table to build a GroupMap (~5 ms for a
                    // 12.5 k-row inner), while per-row execution with a
                    // PK index seek is ~5 µs per call — much cheaper for
                    // N ≤ ~1000 outer rows.
                    let pass_memo = early_cap.is_none_or(|cap| cap > 1000);
                    let memo_arg = if pass_memo { Some(&mut memo) } else { None };
                    values.push(
                        self.eval_expr_with_correlated(&p.expr, row, &ctx, cancel, memo_arg)?,
                    );
                }
                crate::bump_counter!(crate::select::PROJ_ROW_BUILT);
                if stmt.distinct {
                    let bucket = seen_distinct
                        .entry(norm_hash_values(&proj_buf, &distinct_hb, ctx.mysql_dialect))
                        .or_default();
                    if bucket
                        .iter()
                        .any(|i| values_eq_norm(&tagged[i].1.values, &proj_buf, ctx.mysql_dialect))
                    {
                        crate::bump_counter!(crate::select::DISTINCT_DUP_DROPPED);
                        return Ok(());
                    }
                    bucket.push(tagged.len());
                }
                let out = Row::new(core::mem::replace(
                    &mut proj_buf,
                    proj_pool.pop().unwrap_or_default(),
                ));
                let order_keys = if stmt.distinct && !order_by.is_empty() {
                    build_order_keys(&order_by, row, &ctx)?
                } else {
                    order_keys
                };
                budget.charge(approx_row_bytes(&out))?;
                tagged.push((order_keys, out));
            }
            // Streaming top-N: bound the accumulator to O(keep) rows.
            if let Some((k, descs)) = &topk_stream {
                crate::orderby::topk_trim_recycling(
                    &mut tagged,
                    *k,
                    descs,
                    &mut proj_pool,
                    &mut key_pool,
                    &mut topk_boundary,
                );
            }
            Ok(())
        };
        // v7.37.15 (Phase C.3, step 2) — MVCC visibility gate for the
        // load-bearing full-scan path. This is the primary single-table
        // executor; pre-C.3 it read every hot-tier row raw. Once C.3's
        // in-place writers retain dead/old versions, an ungated scan
        // here would return them, so the gate must land BEFORE the
        // writers flip (see the plan's activation-order rule). A no-op
        // today: every hot row is frozen or committed-and-alive under
        // the reader's snapshot, so `is_row_visible` returns true for
        // all of them (verified by the full e2e suite staying green).
        let scan_snapshot = self.current_snapshot();
        let mut emitted: usize = 0;
        if let Some(rows) = &indexed_rows {
            for (loop_idx, cow) in rows.iter().enumerate() {
                if let Some(cap) = early_cap
                    && emitted >= cap
                {
                    break;
                }
                process_row(cow.as_ref(), loop_idx)?;
                emitted = emitted.saturating_add(1);
            }
        } else {
            // v7.39 (round 570) — the row store is a 32-way trie, so
            // indexing it is four dependent loads. Round 567 measured
            // -18% on the aggregate scan from holding the leaf between
            // rows; this is the same loop for the projecting scan.
            let mut rows_cur = table.rows().run_cursor();
            for i in 0..table.row_count() {
                if let Some(cap) = early_cap
                    && emitted >= cap
                {
                    break;
                }
                // Skip rows this snapshot cannot see (invisible rows do
                // not count toward the LIMIT).
                if !table.is_row_visible(i, &scan_snapshot) {
                    continue;
                }
                let Some(row) = rows_cur.get(i) else { continue };
                process_row(row, i)?;
                emitted = emitted.saturating_add(1);
            }
            // v7.35.1 (mailrs prod #6 follow-up) — fold cold-tier
            // rows into the same loop. The full-scan path here is the
            // load-bearing single-table SELECT executor, and pre-
            // 7.35.1 it only walked `table.rows()` (hot), so any
            // `SELECT … FROM t` against a table with cold segments
            // silently returned a subset.
            let cold_rows = self.iter_cold_rows_of_table(table);
            for (offset, row) in cold_rows.iter().enumerate() {
                if let Some(cap) = early_cap
                    && emitted >= cap
                {
                    break;
                }
                process_row(row, table.row_count() + offset)?;
                emitted = emitted.saturating_add(1);
            }
        }

        // (DISTINCT already de-duped STREAMING inside process_row, so the
        // sort below only sees the u survivors and the partial-sort
        // budget applies to DISTINCT too.)
        if !order_by.is_empty() {
            // Partial-sort fast path: when LIMIT is small relative to
            // the row count, select_nth_unstable + sort just the
            // prefix is O(n + k log k) instead of O(n log n).
            // WITH TIES needs the full sort so the tie extension can
            // scan past `limit` to find rows that share the last-kept
            // row's key.
            let keep = if stmt.limit_with_ties
                // v7.38 元机制 D acceptor — `SPG_TEST_DISABLE_TOPK=1`
                // forces the full-sort fallback by suppressing the
                // partial-sort `keep` budget. See
                // `xtests/sigil/test-mode-gucs.md`.
                || self.env_cfg().disable_topk
            {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = order_by.iter().map(|o| o.desc).collect();
            crate::orderby::partial_sort_tagged_in(&mut tagged, keep, &descs, &order_colls);
        }

        // v7.17.0 Phase 3.P0-49 — `FETCH FIRST … WITH TIES` extends
        // past the truncated tail through every row that shares the
        // last-kept row's ORDER BY key. The tie check uses the
        // already-computed `(order_keys, row)` pairs so it matches
        // the sort comparator exactly. DISTINCT + WITH TIES falls
        // through to the no-ties path (PG also disallows their
        // combination; SPG silently drops the tie extension here so
        // the customer doesn't see a hard error mid-query — the
        // user-visible result is still correct, just narrower).
        let output_rows: Vec<Row<'static>> = if stmt.limit_with_ties && !stmt.distinct {
            apply_offset_and_limit_tagged(
                &mut tagged,
                stmt.offset_literal(),
                stmt.limit_literal(),
                true,
            );
            tagged.into_iter().map(|(_, r)| r).collect()
        } else {
            // DISTINCT already de-duped pre-sort above.
            let mut output_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
            apply_offset_and_limit(
                &mut output_rows,
                stmt.offset_literal(),
                stmt.limit_literal(),
            );
            output_rows
        };

        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type;
                c.collation_name = p.collation_name;
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();

        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }

    /// v7.31 (perf — PG lesson #1): shared aggregate finisher. Apply
    /// OFFSET/LIMIT first, then evaluate the deferred subquery-bearing
    /// select items for the surviving rows only — PG's Result-above-
    /// Limit shape, where SubPlan loops equal the OUTPUT row count
    /// (50) instead of the group count (24k).
    fn finish_agg_result(
        &self,
        mut agg: aggregate::AggResult,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        apply_offset_and_limit(&mut agg.rows, stmt.offset_literal(), stmt.limit_literal());
        if !agg.deferred.is_empty() {
            apply_offset_and_limit(
                &mut agg.synth_rows,
                stmt.offset_literal(),
                stmt.limit_literal(),
            );
            let ctx = EvalContext::new(&agg.synth_schema, None);
            let mut memo = memoize::MemoizeCache::default();
            // v7.32 (architecture v2 P3) — keyed index-probe seeding.
            // Deferred subqueries are referenced only by surviving
            // select-list rows (≤ LIMIT), so their correlation keys are
            // exactly the ≤LIMIT group keys in `synth_rows`. Pre-build
            // each batchable subquery's group map over just those keys
            // via per-key index seek; the per-row splice loop below then
            // reuses the seeded map. A join-shaped or un-indexed inner
            // falls through to the all-keys batch inside the call (built
            // eagerly here instead of lazily on row 0 — same cost), so
            // it still pays the full scan, never the 715 ms per-row
            // direct eval; its index-nested-loop probe is the next
            // knife. Genuinely non-batchable shapes return None and are
            // left unseeded for the loop's per-row resolver, as before.
            for (_, expr) in &agg.deferred {
                let mut subs: Vec<&SelectStatement> = Vec::new();
                collect_scalar_subqueries(expr, &mut subs);
                for sub in subs {
                    let repr = alloc::format!("{sub}");
                    if memo.group_maps.contains_key(&repr) {
                        continue;
                    }
                    if let Some(gm) = self.try_batch_correlated_scalar(
                        sub,
                        Some((&agg.synth_rows, &ctx)),
                        cancel,
                    )? {
                        memo.group_maps.insert(repr, Some(alloc::rc::Rc::new(gm)));
                    }
                }
            }
            for (ri, srow) in agg.synth_rows.iter().enumerate() {
                cancel.check()?;
                for (col, expr) in &agg.deferred {
                    let v =
                        self.eval_expr_with_correlated(expr, srow, &ctx, cancel, Some(&mut memo))?;
                    if let Some(cell) = agg.rows[ri].values.get_mut(*col) {
                        *cell = v;
                    }
                }
            }
        }
        Ok(QueryResult::Rows {
            columns: agg.columns,
            rows: agg.rows,
        })
    }

    /// v7.37 — streaming projection for the joined-non-aggregate
    /// shape (multi-table FROM, all projection items bound, no
    /// ORDER BY / DISTINCT / GROUP BY / HAVING / LIMIT / OFFSET /
    /// UNION). Walks the deferred join survivors and emits
    /// `&[&Value]` borrowed straight out of the source tables — no
    /// `.cloned()`, no `Vec<Row<'static>>`. Skips the 25 k × 3-TEXT clone tax
    /// on the mailrs `PROJ` shape (about 4 ms saved).
    ///
    /// Returns `Ok(None)` when the shape doesn't qualify; the caller
    /// then falls back to the materialising path.
    /// v7.37 (round 831) — stream a joinless SELECT straight off the
    /// stored table, one row at a time, without ever building a row set.
    ///
    /// Returns `Ok(None)` for anything this cannot serve, and the caller
    /// falls through to the deferred-join path exactly as before: a
    /// missing table, or a cold tier whose hydration the fallback handles.
    /// Sort a single-table scan through the external sorter, so the
    /// answer's size is bounded by `work_mem` and not by the input.
    ///
    /// Sorting held every row twice — the scan's `Vec<Row>` and the
    /// sort's `Vec<(keys, Row)>` beside it — with nothing bounding
    /// either: 807 MB at 400k rows, whatever `work_mem` said. A large
    /// enough ORDER BY took the server down, which is a liveness
    /// problem before it is a performance one.
    ///
    /// A SEPARATE walk rather than a change to `run_single_table_scan`,
    /// following what round 831 did for the joinless shape. That
    /// function is 552 lines whose projection loop is entangled with
    /// DISTINCT (which indexes back into the tagged vector) and with
    /// streaming top-N (whose boundary moves as the scan runs); both
    /// assume the projection has already happened when a row is
    /// pushed, which is exactly what spilling has to defer. Two earlier
    /// attempts tried to rework that loop and were reverted. Here the
    /// existing path is untouched and this one only claims shapes it
    /// can serve, so a decline costs nothing.
    ///
    /// Records are SOURCE rows, not projected ones: `finish` re-derives
    /// keys from what it decodes, and an ORDER BY key need not be in
    /// the projection — `SELECT pad FROM big ORDER BY id` (round 835).
    fn try_spill_sorted_scan(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        // Shapes this walk does not serve. Each one either needs the
        // whole tagged vector addressable (DISTINCT probes back into
        // it, WITH TIES re-reads its tail) or is already bounded
        // without spilling (a LIMIT makes the partial sort O(keep)).
        if !self.can_spill()
            || stmt.order_by.is_empty()
            || stmt.distinct
            || stmt.limit_with_ties
            || stmt.limit_literal().is_some()
            || !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || select_has_window(stmt)
        {
            return Ok(None);
        }
        // A parent's rows are its children's. These walks scan the named
        // relation alone, so a partitioned or inherited parent comes back
        // short — and silently: the corpus caught `SELECT id FROM pr
        // ORDER BY id` and `SELECT k FROM pl ORDER BY k` returning the
        // parent's own rows instead of the partitions'. `ONLY` is exactly
        // the case that does not fan out, so it stays, which is the test
        // the FROM-clause fan-out itself makes.
        if !from.primary.only
            && crate::partition::has_children(self.active_catalog(), &from.primary.name)
        {
            return Ok(None);
        }
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        // Cold-tier rows live outside `rows()`; this walk would drop
        // them silently, the same reason round 831's walk declines.
        if table.has_cold_rows_fast() {
            return Ok(None);
        }

        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let cols = table.schema().columns.clone();
        let sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&sess);
        let projection = build_projection(&stmt.items, &cols, alias, self.backslash_escapes)?;
        let order_by = stmt.order_by.clone();
        // The same one-shot resolution the general path does (round
        // 582): each ORDER BY column is bound once, not once per row.
        let order_bound = crate::orderby::order_by_bound_positions(&order_by, &cols, Some(alias));
        let descs: Vec<bool> = order_by.iter().map(|o| o.desc).collect();
        // Resolved BEFORE the scan, because it now decides what the sort
        // STORES and not just what it decodes (round 995).
        let needed = Self::sort_record_columns_needed(&stmt.items, &order_bound, cols.len(), &ctx);

        let mut sorter = crate::extsort::ExternalSorter::new(
            self.temp_run_factory,
            self.session_work_mem_bytes(),
            cols.clone(),
            &descs,
        )
        .with_stats(&self.spill_stats)
        .with_pruned(&needed);
        let snapshot = self.current_snapshot();
        // One key buffer for the whole scan: `push` drains it and leaves
        // the capacity behind.
        let mut keys: Vec<OrderKey> = Vec::new();
        // r1024 — compile the predicate once for the scan.
        //
        // These two sorted-spill scans are the paths a single-table SELECT
        // with an ORDER BY takes, and they were the last row-returning ones
        // still walking the expression tree per row. r1023 did the
        // no-ORDER-BY sibling; the sweep's two remaining losing cells are
        // exactly this shape.
        //
        // Found from the profile's CALL TREE rather than its leaves. The
        // leaves say what is expensive — `eval_expr` 320, `apply_binary`
        // 261, `mod_op` 178 — and two attempts at reasoning out which
        // function asked for it were both wrong. The tree names the caller
        // chain, and it named this one.
        let compiled_where: Option<crate::eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| crate::eval::fully_compilable(w))
            .map(|w| crate::eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        for (i, row) in table.scan_visible_from(0, &snapshot) {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(c) = &compiled_where {
                if !crate::eval::compiled::eval_compiled_pred(
                    c,
                    row,
                    &ctx,
                    &mut eval_stack,
                    ctx.mysql_dialect,
                )? {
                    continue;
                }
            } else if let Some(w) = &stmt.where_ {
                let cond = crate::eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    continue;
                }
            }
            keys.clear();
            crate::orderby::build_order_keys_bound(&order_by, &order_bound, row, &ctx, &mut keys)?;
            sorter.push(&mut keys, row)?;
        }

        let key_ctx = &ctx;
        let rows = sorter.finish(
            |src, buf| {
                crate::orderby::build_order_keys_bound(&order_by, &order_bound, src, key_ctx, buf)
            },
            |src| {
                let mut values = Vec::with_capacity(projection.len());
                for p in &projection {
                    values.push(
                        crate::eval::eval_expr(&p.expr, src, key_ctx).map_err(EngineError::Eval)?,
                    );
                }
                Ok(Row::new(values))
            },
        )?;

        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        Ok(Some(QueryResult::Rows { columns, rows }))
    }

    /// v7.37 (round 882) — the bounded sort of `try_spill_sorted_scan`,
    /// handing each row to the consumer instead of collecting the answer.
    ///
    /// That walk bounds the SORT and then returns `QueryResult::Rows`,
    /// which holds every output row. Measured at `work_mem = 4 MB` over
    /// 200-byte rows, RSS above the server's own baseline while the
    /// query runs grew +30 MB at 100k rows, +68 MB at 200k and +137 MB
    /// at 400k — linear — while the spill underneath worked correctly
    /// (9 / 17 / 33 runs, witnessed DURING the query; `FileRun::drop`
    /// removes each file, so a count taken afterwards reads 0 whatever
    /// happened, and an earlier reading of "no spill at all" was that
    /// blind witness). The growth is the collected result, not the sort.
    ///
    /// Emitting makes peak the budget, one buffer per run and a single
    /// row — the state a merge already holds at every step. It also
    /// frees each projected row as the next is built rather than
    /// accumulating them, which is where the time is: a profile of the
    /// collecting walk put the allocator at 586 samples, more than every
    /// sort comparison combined (420), against 19 for `push` itself.
    /// v7.37 (round 923) — which of a sort record's columns the output half
    /// reads. The record is the SOURCE row (round 836), so a narrow projection
    /// decoded every column: skipping one 200-byte text halves a decode
    /// (2.17 -> 1.14 ms per pass at 10k rows, priced additively).
    ///
    /// Timid on purpose — a wrong mask is a SILENT wrong answer, a pruned
    /// column reads NULL. Answers only when every projection item is a bare
    /// column reference AND every ORDER BY key is a bound column; anything
    /// else returns empty, decoding everything as before.
    /// `explain.rs`'s `collect_column_refs` is NOT used: its `_ => {}` arm
    /// drops references from expression kinds it does not enumerate.
    ///
    /// ORDER BY columns are included — the merge re-derives keys from the
    /// decoded row on the spilled path, so pruning one would sort NULLs.
    pub(crate) fn sort_record_columns_needed(
        items: &[SelectItem],
        order_bound: &[Option<usize>],
        arity: usize,
        ctx: &EvalContext,
    ) -> Vec<bool> {
        let all_bare = items.iter().all(|i| {
            matches!(
                i,
                SelectItem::Expr {
                    expr: Expr::Column(_),
                    ..
                }
            )
        });
        if !all_bare || order_bound.iter().any(Option::is_none) {
            return Vec::new();
        }
        let mut mask = alloc::vec![false; arity];
        for item in items {
            if let SelectItem::Expr {
                expr: Expr::Column(c),
                ..
            } = item
            {
                match crate::eval::find_column_pos(c, ctx) {
                    Some(p) if p < arity => mask[p] = true,
                    _ => return Vec::new(),
                }
            }
        }
        for p in order_bound.iter().flatten() {
            if *p < arity {
                mask[*p] = true;
            } else {
                return Vec::new();
            }
        }
        mask
    }

    /// r1025 — `ORDER BY <indexed NOT NULL column>` walks the index instead
    /// of sorting.
    ///
    /// PG serves such an ordering from the index and never sorts. We sorted:
    /// measured at 400,000 rows, `SELECT pad FROM t ORDER BY id` costs
    /// 138-144 ms against PG18's 64-75, and the call tree puts the cost in
    /// the sorter's own round trip — `ExternalSorter::finish_each` →
    /// `next_row` → `decode_row_body_dense_pruned` → `read_value_body`.
    /// Every row is encoded into the sorter's arena and decoded back out,
    /// for an order the index already holds.
    ///
    /// The walk exists — `try_pk_walk_top_n` — and requires a `LIMIT`,
    /// because it was built for top-N. This is the unbounded sibling.
    ///
    /// NOT NULL is a hard gate, not a simplification: a NULL key is absent
    /// from a btree, so walking one would silently drop those rows. That is
    /// exactly the defect r1020 fixed on the top-N path, where it had
    /// shipped.
    fn try_index_order_stream<F>(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        // The same shape gates the spill sort applies, minus `can_spill`:
        // this path never spills.
        if stmt.order_by.len() != 1
            || stmt.distinct
            || stmt.limit_with_ties
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.having.is_some()
            || stmt.group_by.is_some()
            || !stmt.unions.is_empty()
            || !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.as_of_segment.is_some()
            || from.primary.generate_series_args.is_some()
            || select_has_window(stmt)
            || aggregate::uses_aggregate(stmt)
        {
            return Ok(None);
        }
        if stmt
            .items
            .iter()
            .any(|i| matches!(i, SelectItem::Expr { expr, .. } if is_top_level_unnest(expr)))
        {
            return Ok(None);
        }
        crate::orderby::check_order_by_legality(stmt)?;
        crate::orderby::check_order_by_positions(stmt)?;
        crate::window::reject_window_in_row_clauses(stmt)?;
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        // Cold rows are reachable through locators, but the walk would have
        // to resolve them per key; the ordinary path already covers that.
        if table.has_cold_rows_fast() {
            return Ok(None);
        }
        if !from.primary.only
            && crate::partition::has_children(self.active_catalog(), &from.primary.name)
        {
            return Ok(None);
        }
        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let cols = table.schema().columns.clone();

        let order = &stmt.order_by[0];
        let Expr::Column(oc) = &order.expr else {
            return Ok(None);
        };
        if let Some(q) = &oc.qualifier
            && !q.eq_ignore_ascii_case(alias)
        {
            return Ok(None);
        }
        let Some(order_pos) = cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&oc.name))
        else {
            return Ok(None);
        };
        // See the NOT NULL note above: this is the r1020 defect's gate.
        if cols[order_pos].nullable {
            return Ok(None);
        }
        let Some(index) = table.index_on(order_pos) else {
            return Ok(None);
        };
        if !matches!(index.kind, spg_storage::IndexKind::BTree(_))
            || index.expression.is_some()
            || index.partial_predicate.is_some()
        {
            return Ok(None);
        }

        let sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&sess);
        let projection = build_projection(&stmt.items, &cols, alias, self.backslash_escapes)?;
        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        emit(crate::StreamItem::Header(&columns))?;
        let bound_pos: Vec<Option<usize>> = projection
            .iter()
            .map(|p| match &p.expr {
                Expr::Column(c) => match crate::eval::locate_column(c, &ctx) {
                    Ok(Some(pos)) => Some(pos),
                    _ => None,
                },
                _ => None,
            })
            .collect();

        let compiled_where: Option<crate::eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| crate::eval::fully_compilable(w))
            .map(|w| crate::eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        let mut values: Vec<Value<'static>> = Vec::with_capacity(projection.len());
        let snapshot = self.current_snapshot();

        // A btree holds one locator per row VERSION, so a row whose key was
        // updated can sit under two keys and a dead one can sit beside its
        // replacement. The visibility gate drops the dead; `seen` drops a
        // live row that the walk reaches twice, which would otherwise be a
        // duplicated output row rather than a slow one.
        let mut emitted_rows = alloc::vec![false; table.rows().len()];
        let walker: alloc::boxed::Box<
            dyn Iterator<Item = (&spg_storage::IndexKey, &spg_storage::PostingList)>,
        > = if order.desc {
            alloc::boxed::Box::new(index.iter_desc())
        } else {
            alloc::boxed::Box::new(index.iter_asc())
        };
        let mut count = 0usize;
        let mut visited = 0usize;
        for (_key, locators) in walker {
            for loc in locators {
                let spg_storage::RowLocator::Hot(ri) = *loc else {
                    continue;
                };
                if emitted_rows.get(ri).copied().unwrap_or(true) {
                    continue;
                }
                if !table.is_row_visible(ri, &snapshot) {
                    continue;
                }
                let Some(row) = table.rows().get(ri) else {
                    continue;
                };
                visited += 1;
                if visited.is_multiple_of(256) {
                    cancel.check()?;
                }
                emitted_rows[ri] = true;
                if Self::stream_project_row(
                    row,
                    stmt.where_.as_ref(),
                    compiled_where.as_ref(),
                    &mut eval_stack,
                    &projection,
                    &bound_pos,
                    &ctx,
                    &mut values,
                    emit,
                )? {
                    count += 1;
                }
            }
        }
        Ok(Some(count))
    }

    /// r1031 — `ORDER BY` over NOT NULL integer columns, sorted without
    /// building an `OrderKey` vector per row.
    ///
    /// The row-returning sorted scan allocates twice per row: one
    /// `Vec<OrderKey>` for the sort keys and one `Vec<Value>` for the
    /// projection. Counted over 400 k rows (r1030,
    /// `docs/PERF_SORTED_SCAN_ALLOCATIONS_2026-08-15.md`), that is 800,067
    /// allocations and 208 MB of traffic for an answer of four hundred
    /// thousand integers.
    ///
    /// The key half is pure ceremony on this shape.
    /// `sort_tagged_by_inline_int_key` already sorts indices rather than
    /// rows, so the per-row vector is built, has one integer taken out of
    /// it, and is then dragged through the permutation — it exists to carry
    /// a number the row's column already held. This lane carries the number
    /// instead, in a fixed-size array that lives inside the buffer element
    /// and allocates nothing. Same idea as the predicate VM's integer lane.
    ///
    /// Declines to `None` for anything it does not cover, and every caller
    /// falls through to the general path, so the gate list is the
    /// specification.
    ///
    /// Ties: equal keys keep scan order, as the stable sort on the general
    /// path does. Rows that tie on every ORDER BY term are entitled to any
    /// order among themselves either way — see `STABILITY.md`.
    fn try_int_key_sorted_stream<F>(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        /// Sort terms this lane carries inline. Four covers every ORDER BY
        /// in the endpoint sweep and in the dogfood corpus; wider ones fall
        /// through rather than growing the buffer element for everybody.
        const MAX_KEYS: usize = 4;

        if stmt.order_by.is_empty()
            || stmt.order_by.len() > MAX_KEYS
            || stmt.distinct
            || stmt.limit_with_ties
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.having.is_some()
            || stmt.group_by.is_some()
            || !stmt.unions.is_empty()
            || !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.as_of_segment.is_some()
            || from.primary.generate_series_args.is_some()
            || select_has_window(stmt)
            || aggregate::uses_aggregate(stmt)
        {
            return Ok(None);
        }
        if stmt
            .items
            .iter()
            .any(|i| matches!(i, SelectItem::Expr { expr, .. } if is_top_level_unnest(expr)))
        {
            return Ok(None);
        }
        crate::orderby::check_order_by_legality(stmt)?;
        crate::orderby::check_order_by_positions(stmt)?;
        crate::window::reject_window_in_row_clauses(stmt)?;
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        if table.has_cold_rows_fast() {
            return Ok(None);
        }
        if !from.primary.only
            && crate::partition::has_children(self.active_catalog(), &from.primary.name)
        {
            return Ok(None);
        }
        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let cols = table.schema().columns.clone();

        // Every ORDER BY term must be a NOT NULL integer column of this
        // table. NOT NULL is what lets the key be a bare integer: with
        // NULLs the lane would have to carry their ordering too, and
        // getting that subtly wrong is the r1020 defect.
        let mut key_pos = [0usize; MAX_KEYS];
        let mut descs = [false; MAX_KEYS];
        // PG's default is NULLS LAST for ASC and NULLS FIRST for DESC,
        // which the AST records as `None`; `unwrap_or(desc)` is how the
        // rest of the engine resolves it.
        let mut nulls_first = [false; MAX_KEYS];
        let n_keys = stmt.order_by.len();
        for (slot, order) in stmt.order_by.iter().enumerate() {
            let Expr::Column(oc) = &order.expr else {
                return Ok(None);
            };
            if let Some(q) = &oc.qualifier
                && !q.eq_ignore_ascii_case(alias)
            {
                return Ok(None);
            }
            let Some(pos) = cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&oc.name))
            else {
                return Ok(None);
            };
            if !matches!(
                cols[pos].ty,
                spg_storage::DataType::SmallInt
                    | spg_storage::DataType::Int
                    | spg_storage::DataType::BigInt
            ) {
                return Ok(None);
            }
            key_pos[slot] = pos;
            descs[slot] = order.desc;
            nulls_first[slot] = order.nulls_first.unwrap_or(order.desc);
        }

        let sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&sess);
        let projection = build_projection(&stmt.items, &cols, alias, self.backslash_escapes)?;
        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        let bound_pos: Vec<Option<usize>> = projection
            .iter()
            .map(|p| match &p.expr {
                Expr::Column(c) => match crate::eval::locate_column(c, &ctx) {
                    Ok(Some(pos)) => Some(pos),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let compiled_where: Option<crate::eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| crate::eval::fully_compilable(w))
            .map(|w| crate::eval::compile_expr(w, &ctx));

        // The same first-observable point the materialising planner fires,
        // placed after the gates so it fires exactly once: this lane runs
        // BEFORE that planner and would otherwise be a hole in the
        // panic-isolation and cancellation-race coverage rather than a
        // faster path through it.
        crate::injection_point!("planner_first_row_fetch", &stmt.from);

        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        let mut values: Vec<Value<'static>> = Vec::with_capacity(projection.len());
        let mut budget = ByteBudget::new(self.max_query_bytes);
        let snapshot = self.current_snapshot();
        // Keys, a NULL bit per key slot, and the row. The bitmask keeps
        // the element small: a nullable key still costs one bit rather
        // than a second array.
        let mut sorted: Vec<([i64; MAX_KEYS], u8, Vec<Value<'static>>)> = Vec::new();

        for (ri, row) in table.rows().iter().enumerate() {
            if ri.is_multiple_of(256) {
                cancel.check()?;
            }
            if !table.is_row_visible(ri, &snapshot) {
                continue;
            }
            // The key comes from the STORED row, before projection: an
            // ORDER BY column need not appear in the select list.
            let mut keys = [0i64; MAX_KEYS];
            let mut nulls = 0u8;
            let mut keyed = true;
            for slot in 0..n_keys {
                match row.values.get(key_pos[slot]) {
                    Some(Value::SmallInt(v)) => keys[slot] = i64::from(*v),
                    Some(Value::Int(v)) => keys[slot] = i64::from(*v),
                    Some(Value::BigInt(v)) => keys[slot] = *v,
                    Some(Value::Null) | None => nulls |= 1 << slot,
                    // An integer column holding something else is a row
                    // this lane cannot order; hand the whole query back
                    // rather than guess at it.
                    _ => {
                        keyed = false;
                        break;
                    }
                }
            }
            if !keyed {
                return Ok(None);
            }
            if !Self::stream_filter_project(
                row,
                stmt.where_.as_ref(),
                compiled_where.as_ref(),
                &mut eval_stack,
                &projection,
                &bound_pos,
                &ctx,
                &mut values,
            )? {
                continue;
            }
            budget.charge(crate::bytebudget::approx_values_bytes(&values))?;
            sorted.push((keys, nulls, core::mem::take(&mut values)));
            values.reserve(projection.len());
        }

        sorted.sort_by(|a, b| {
            use core::cmp::Ordering;
            for slot in 0..n_keys {
                let bit = 1u8 << slot;
                let ord = match (a.1 & bit != 0, b.1 & bit != 0) {
                    (true, true) => Ordering::Equal,
                    // Where the NULLs go is already decided — `nulls_first`
                    // resolved DESC's default when it was read. Reversing
                    // this for DESC as well would apply the direction
                    // twice and put them at the wrong end.
                    (true, false) => {
                        if nulls_first[slot] {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if nulls_first[slot] {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (false, false) => {
                        let o = a.0[slot].cmp(&b.0[slot]);
                        if descs[slot] { o.reverse() } else { o }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });

        emit(crate::StreamItem::Header(&columns))?;
        let count = sorted.len();
        for (_, _, vals) in &sorted {
            emit(crate::StreamItem::Row(crate::RowCells::Values(vals)))?;
        }
        Ok(Some(count))
    }

    fn try_spill_sorted_stream<F>(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        // The shapes `try_spill_sorted_scan` declines, plus the ones the
        // streaming executor does not carry (a LIMIT is already bounded
        // by a partial sort; the rest need the answer addressable).
        if !self.can_spill()
            || stmt.order_by.is_empty()
            || stmt.distinct
            || stmt.limit_with_ties
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.having.is_some()
            || stmt.group_by.is_some()
            || !stmt.unions.is_empty()
            || !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.as_of_segment.is_some()
            || from.primary.generate_series_args.is_some()
            || select_has_window(stmt)
            || aggregate::uses_aggregate(stmt)
        {
            return Ok(None);
        }
        if stmt
            .items
            .iter()
            .any(|i| matches!(i, SelectItem::Expr { expr, .. } if is_top_level_unnest(expr)))
        {
            return Ok(None);
        }
        // Everything `exec_bare_select_cancel` does before it scans runs
        // BELOW this path, so a statement claimed here skips it. Three of
        // those were missed on the way in and each was caught by a
        // different gate — the ORDER BY rules by an e2e (`SELECT a FROM t
        // ORDER BY 2` sorted happily instead of raising 42P10), the
        // cancellation check by another, the partition fan-out by the
        // differential corpus. What is reconciled, item by item: with-ties
        // needs ORDER BY (gated above), USING/NATURAL and RLS join
        // rewrites (joins gated above), the single-table RLS predicate
        // (the dispatcher declines a policy-subject table before this is
        // reached), the meta-view dispatch (those names are not in the
        // catalog, so the lookup below declines). These three are calls,
        // so the message and SQLSTATE are the ones the fall-back gives —
        // `select_has_window` above reads the select list and ORDER BY but
        // not WHERE, which is the case the third one covers.
        crate::orderby::check_order_by_legality(stmt)?;
        crate::orderby::check_order_by_positions(stmt)?;
        crate::window::reject_window_in_row_clauses(stmt)?;
        // A parent's rows are its children's. These walks scan the named
        // relation alone, so a partitioned or inherited parent comes back
        // short — and silently: the corpus caught `SELECT id FROM pr
        // ORDER BY id` and `SELECT k FROM pl ORDER BY k` returning the
        // parent's own rows instead of the partitions'. `ONLY` is exactly
        // the case that does not fan out, so it stays, which is the test
        // the FROM-clause fan-out itself makes.
        if !from.primary.only
            && crate::partition::has_children(self.active_catalog(), &from.primary.name)
        {
            return Ok(None);
        }
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        // Cold-tier rows live outside `rows()`; this walk would drop
        // them silently, the same reason round 831's walk declines.
        if table.has_cold_rows_fast() {
            return Ok(None);
        }

        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let cols = table.schema().columns.clone();
        let sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&sess);
        let projection = build_projection(&stmt.items, &cols, alias, self.backslash_escapes)?;
        let order_by = stmt.order_by.clone();
        // The same one-shot resolution the general path does (round
        // 582): each ORDER BY column is bound once, not once per row.
        let order_bound = crate::orderby::order_by_bound_positions(&order_by, &cols, Some(alias));
        let descs: Vec<bool> = order_by.iter().map(|o| o.desc).collect();
        // Resolved BEFORE the scan, because it now decides what the sort
        // STORES and not just what it decodes (round 995).
        let needed = Self::sort_record_columns_needed(&stmt.items, &order_bound, cols.len(), &ctx);

        let mut sorter = crate::extsort::ExternalSorter::new(
            self.temp_run_factory,
            self.session_work_mem_bytes(),
            cols.clone(),
            &descs,
        )
        .with_stats(&self.spill_stats)
        .with_pruned(&needed);
        let snapshot = self.current_snapshot();
        // One key buffer for the whole scan: `push` drains it and leaves
        // the capacity behind.
        let mut keys: Vec<OrderKey> = Vec::new();
        // r1024 — compile the predicate once for the scan.
        //
        // These two sorted-spill scans are the paths a single-table SELECT
        // with an ORDER BY takes, and they were the last row-returning ones
        // still walking the expression tree per row. r1023 did the
        // no-ORDER-BY sibling; the sweep's two remaining losing cells are
        // exactly this shape.
        //
        // Found from the profile's CALL TREE rather than its leaves. The
        // leaves say what is expensive — `eval_expr` 320, `apply_binary`
        // 261, `mod_op` 178 — and two attempts at reasoning out which
        // function asked for it were both wrong. The tree names the caller
        // chain, and it named this one.
        let compiled_where: Option<crate::eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| crate::eval::fully_compilable(w))
            .map(|w| crate::eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        for (i, row) in table.scan_visible_from(0, &snapshot) {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(c) = &compiled_where {
                if !crate::eval::compiled::eval_compiled_pred(
                    c,
                    row,
                    &ctx,
                    &mut eval_stack,
                    ctx.mysql_dialect,
                )? {
                    continue;
                }
            } else if let Some(w) = &stmt.where_ {
                let cond = crate::eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    continue;
                }
            }
            keys.clear();
            crate::orderby::build_order_keys_bound(&order_by, &order_bound, row, &ctx, &mut keys)?;
            sorter.push(&mut keys, row)?;
        }

        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        emit(crate::StreamItem::Header(&columns))?;

        let key_ctx = &ctx;
        let mut emitted_since_check = 0usize;
        let n = sorter.finish_each(
            |src, buf| {
                crate::orderby::build_order_keys_bound(&order_by, &order_bound, src, key_ctx, buf)
            },
            |src, values| {
                for p in &projection {
                    values.push(
                        crate::eval::eval_expr(&p.expr, src, key_ctx).map_err(EngineError::Eval)?,
                    );
                }
                Ok(())
            },
            |cells| {
                // The merge is the long half of a big sort, and the scan's
                // check above stops running once it ends: a cancelled
                // `SELECT pad FROM big ORDER BY id` delivered all 120k rows
                // anyway. Same stride as the scan.
                emitted_since_check += 1;
                if emitted_since_check >= 256 {
                    emitted_since_check = 0;
                    cancel.check()?;
                }
                emit(crate::StreamItem::Row(crate::RowCells::Values(cells)))
            },
        )?;
        Ok(Some(n))
    }

    /// One row of the single-table streaming walk: the WHERE test, the
    /// projection, the emit. Returns whether a row was emitted.
    ///
    /// v7.39 (round 970) — factored out because the walk now has two ways
    /// to reach a row, the sequential scan and an index seek's candidate
    /// positions, and both must do IDENTICALLY this. A copy in each is how
    /// two paths for one job drift; this file already carries the cost of
    /// that lesson twice (rounds 823 and 961, both resolvers).
    ///
    /// `#[inline]` so the scan loop keeps the shape round 957 measured it
    /// in — a shared hot path pays for a new abstraction whether or not it
    /// uses it, and this one is on the scan.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn stream_filter_project(
        row: &spg_storage::Row<'static>,
        where_: Option<&Expr>,
        // r1023 — the same WHERE, compiled once by the caller. `None` means
        // the expression did not qualify and `where_` is evaluated as before.
        compiled_where: Option<&crate::eval::CompiledExpr>,
        eval_stack: &mut Vec<Value<'static>>,
        projection: &[ProjectedItem],
        bound_pos: &[Option<usize>],
        ctx: &crate::eval::EvalContext<'_>,
        values: &mut Vec<Value<'static>>,
    ) -> Result<bool, EngineError> {
        // r1023 — this scan ran its predicate through the TREE INTERPRETER,
        // once per row, and it was the only row-returning path that did.
        // The aggregate path, `table_access`, and the PK walker all compile
        // theirs. Profiled: on `SELECT pad FROM d WHERE id % 3 = 0` the
        // server's live samples were `eval_expr` 99, `apply_binary` 81,
        // `mod_op` 29 — the interpreter, not delivery.
        //
        // The arithmetic accounted for it exactly. Over the wire, the same
        // filter costs 6.375 ms returning rows and 0.679 ms counting them;
        // the 5.70 ms difference over 50,000 scanned rows is 114 ns each,
        // which is what an interpreted predicate costs against the compiled
        // lane's 11.7. It was named "delivery after a filter" before this
        // profile, and it was never delivery.
        if let Some(c) = compiled_where {
            if !crate::eval::compiled::eval_compiled_pred(
                c,
                row,
                ctx,
                eval_stack,
                ctx.mysql_dialect,
            )? {
                return Ok(false);
            }
        } else if let Some(w) = where_ {
            let cond = crate::eval::eval_expr(w, row, ctx).map_err(EngineError::Eval)?;
            if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                return Ok(false);
            }
        }
        values.clear();
        for (p, bound) in projection.iter().zip(bound_pos) {
            values.push(match bound {
                Some(pos) => crate::eval::column_at(*pos, row, ctx).map_err(EngineError::Eval)?,
                None => crate::eval::eval_expr(&p.expr, row, ctx).map_err(EngineError::Eval)?,
            });
        }
        Ok(true)
    }

    /// The same filter and projection, then emit. Split from
    /// [`Self::stream_filter_project`] so a path that has to BUFFER rows
    /// before it can emit them — a sort — runs the identical predicate and
    /// projection rather than a second copy of them.
    #[allow(clippy::too_many_arguments)]
    fn stream_project_row<F>(
        row: &spg_storage::Row<'static>,
        where_: Option<&Expr>,
        compiled_where: Option<&crate::eval::CompiledExpr>,
        eval_stack: &mut Vec<Value<'static>>,
        projection: &[ProjectedItem],
        bound_pos: &[Option<usize>],
        ctx: &crate::eval::EvalContext<'_>,
        values: &mut Vec<Value<'static>>,
        emit: &mut F,
    ) -> Result<bool, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        if !Self::stream_filter_project(
            row,
            where_,
            compiled_where,
            eval_stack,
            projection,
            bound_pos,
            ctx,
            values,
        )? {
            return Ok(false);
        }
        emit(crate::StreamItem::Row(crate::RowCells::Values(values)))?;
        Ok(true)
    }

    fn try_stream_single_table<F>(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        let Some(table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        // Cold-tier rows live outside `rows()`; the materialising fallback
        // covers both tiers and this walk would silently drop them.
        if table.has_cold_rows_fast() {
            return Ok(None);
        }
        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let cols = table.schema().columns.clone();
        let sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&sess);
        let projection = build_projection(&stmt.items, &cols, alias, self.backslash_escapes)?;

        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        emit(crate::StreamItem::Header(&columns))?;

        // v7.37 (round 957) — resolve each bare-column projection ONCE
        // instead of once per row. `find_column_pos`-style resolution is a
        // linear walk of the schema comparing column-name strings, and the
        // row loop below ran it for every cell of every row: measured at
        // 400k rows, binding it out of the loop took `SELECT pad` from
        // 16.5-17.5 ms to 10.9-11.7 ms (-41%, two windows, round 954).
        //
        // ORDER BY has bound its keys this way since round 582
        // (`order_by_bound_positions`); the projection never did.
        //
        // `locate_column` is the same resolution `resolve_column` performs,
        // returning the site instead of the value, so the two cannot drift
        // apart the way a second hand-written resolver would. Anything it
        // declines — an expression, a whole-row reference, a name that does
        // not resolve — binds to `None` and takes the general path below,
        // errors included, so an empty table still reports nothing rather
        // than raising at bind time.
        let bound_pos: Vec<Option<usize>> = projection
            .iter()
            .map(|p| match &p.expr {
                Expr::Column(c) => match crate::eval::locate_column(c, &ctx) {
                    Ok(Some(pos)) => Some(pos),
                    _ => None,
                },
                _ => None,
            })
            .collect();

        // One snapshot for the whole scan, as the materialising path takes.
        let snapshot = self.current_snapshot();

        // v7.39 (round 970) — ask the indices BEFORE walking the table.
        //
        // This walk had no index step at all, and it is preferred over the
        // materialising path, which does have one (`pick_indexed_rows` ->
        // `try_index_seek`). So a primary-key point lookup — the commonest
        // statement there is — read every row: measured on 500k rows,
        // `SELECT * FROM big WHERE id = 250000` took 14.947 ms against
        // PG18.4's 0.172 ms, and the cost tracked the TABLE (1k 0.315 ms,
        // 10k 1.660, 100k 3.518), which is not what O(log n) looks like.
        //
        // The control that named it: `... OFFSET 0` — semantically the same
        // query — answered in 0.159 ms, because OFFSET is one of the shape
        // gates that declines this walk and sends the statement to the path
        // that seeks. `LIMIT 1` and `GROUP BY` did the same. The three have
        // no semantics in common; what they share is making this function
        // stand down.
        //
        // The seek only NARROWS: every candidate still goes through the
        // full WHERE below, exactly as the mutation paths use it, so a
        // partial index match cannot change an answer. Positions come back
        // already visibility-filtered and already capped at a quarter of the
        // table (round 490), so a seek can never cost more than the scan it
        // replaces, and `None` means "walk the table" as before.
        //
        // Sorted because the scan would have produced table order and the
        // index produces key order. Without an ORDER BY neither is promised,
        // but a walk that silently reorders its answer when an index happens
        // to exist is a difference nobody asked for.
        let seek_positions: Option<Vec<usize>> = stmt.where_.as_ref().and_then(|w| {
            crate::index_access::try_index_seek_positions(w, &cols, table, alias, &snapshot)
        });

        let mut values: Vec<Value<'static>> = Vec::with_capacity(projection.len());
        // r1023 — compile the predicate once for the whole scan. Same gate
        // every other path uses: `fully_compilable` or keep the interpreter,
        // so a shape the VM cannot take answers exactly as it did before.
        let compiled_where: Option<crate::eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| crate::eval::fully_compilable(w))
            .map(|w| crate::eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        let mut count: usize = 0;
        match seek_positions {
            Some(mut positions) => {
                positions.sort_unstable();
                for (n, pos) in positions.into_iter().enumerate() {
                    if n.is_multiple_of(256) {
                        cancel.check()?;
                    }
                    let Some(row) = table.rows().get(pos) else {
                        continue;
                    };
                    if Self::stream_project_row(
                        row,
                        stmt.where_.as_ref(),
                        compiled_where.as_ref(),
                        &mut eval_stack,
                        &projection,
                        &bound_pos,
                        &ctx,
                        &mut values,
                        emit,
                    )? {
                        count += 1;
                    }
                }
            }
            None => {
                for (i, row) in table.scan_visible_from(0, &snapshot) {
                    if i.is_multiple_of(256) {
                        cancel.check()?;
                    }
                    if Self::stream_project_row(
                        row,
                        stmt.where_.as_ref(),
                        compiled_where.as_ref(),
                        &mut eval_stack,
                        &projection,
                        &bound_pos,
                        &ctx,
                        &mut values,
                        emit,
                    )? {
                        count += 1;
                    }
                }
            }
        }
        Ok(Some(count))
    }

    pub(crate) fn try_exec_joined_streaming<F>(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<Option<usize>, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        // Shape gates — keep the streamable surface narrow on
        // purpose. The fall-back path still handles everything else.
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        // v7.37 (round 830) — decline anything a row-security policy binds
        // for this session. Policies are injected in
        // `exec_bare_select_cancel`, below this path, so a statement claimed
        // here would read the table unfiltered: measured, `SELECT val FROM
        // sec` returned all three rows to a session whose policy allows two,
        // while `SELECT upper(val) FROM sec` — declined by the shape gates
        // and so materialised — returned the correct two.
        //
        // Declining sends it to the path that enforces. Teaching this one to
        // inject the predicate itself would keep the streaming benefit for
        // RLS tables and is the better end state; it is not what a
        // correctness fix should carry, and the fall-back is exactly as
        // correct, only slower.
        if self.select_reads_policy_subject_table(stmt) {
            return Ok(None);
        }
        // v7.39 (round 790) — single-table SELECTs stream too. This
        // gate said "joins only" because the path was written for
        // mailrs's joined PROJ shape; a plain `SELECT <cols> FROM t`
        // fell to the materialising fallback, which builds the whole
        // `Vec<Row<'static>>` and only then iterates it. Measured on
        // 300k rows: 181 MB single-table vs 70 MB for the SAME rows
        // reached through a one-row JOIN — 2.6x, purely for lacking a
        // join. The deferred-join structure handles one source as the
        // degenerate stride-1 case, so the walk below is unchanged.
        let _single_table = from.joins.is_empty();
        // An ORDER BY that the bounded sort can serve streams; everything
        // else still falls to the materialising fallback below.
        // r1025 — an ordering the index already holds needs no sort at all.
        // Tried before the spill sort, which is the path it replaces.
        if !stmt.order_by.is_empty()
            && from.joins.is_empty()
            && let Some(n) = self.try_index_order_stream(stmt, from, cancel, emit)?
        {
            return Ok(Some(n));
        }
        if !stmt.order_by.is_empty()
            && from.joins.is_empty()
            && let Some(n) = self.try_spill_sorted_stream(stmt, from, cancel, emit)?
        {
            return Ok(Some(n));
        }
        // r1031 — integer keys carried inline instead of an `OrderKey`
        // vector per row. Tried AFTER the spill sort on purpose: this lane
        // buffers the whole answer, so anything the spill path would take
        // must keep taking it rather than be turned back into an in-memory
        // sort that answers with a budget error.
        if !stmt.order_by.is_empty()
            && from.joins.is_empty()
            && let Some(n) = self.try_int_key_sorted_stream(stmt, from, cancel, emit)?
        {
            return Ok(Some(n));
        }
        if !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.having.is_some()
            || stmt.group_by.is_some()
            || stmt.distinct
            || !stmt.unions.is_empty()
            || stmt.limit_with_ties
        {
            return Ok(None);
        }
        if aggregate::uses_aggregate(stmt) {
            return Ok(None);
        }
        // No window / SRF on the streaming path.
        if select_has_window(stmt) {
            return Ok(None);
        }
        if stmt
            .items
            .iter()
            .any(|i| matches!(i, SelectItem::Expr { expr, .. } if is_top_level_unnest(expr)))
        {
            return Ok(None);
        }
        // v7.37 (round 831) — a joinless FROM over a plain stored table
        // never needs the deferred structure, and building one costs the
        // whole table. `materialise_table_ref_filtered` clones every row
        // into a `Vec<Row<'static>>` before anything is filtered or
        // projected, so peak cost tracks the TABLE, not the result:
        // measured over 300k rows of 200 bytes, `SELECT id FROM big` and
        // `SELECT pad FROM big` both cost +107 MB over baseline, the narrow
        // projection saving nothing, while an arithmetic projection — which
        // the shape gates decline, so it materialises through the ordinary
        // executor — cost +21 MB.
        //
        // Scanning in batches and releasing each one is what `cursor_fill`
        // already does for a lazy cursor, and it is the same walk: resume
        // from a slot, take visible rows, evaluate, hand them over, drop
        // them. Round 800's finding stands and is why this reads rows OUT
        // rather than seeding the join by index — touching the stored
        // `PersistentVec` in place makes the whole table resident, which is
        // worse than the copy. Each batch is copied, then freed.
        if from.joins.is_empty()
            && from.primary.unnest_expr.is_none()
            && from.primary.lateral_subquery.is_none()
            && from.primary.as_of_segment.is_none()
            && from.primary.generate_series_args.is_none()
            && let Some(n) = self.try_stream_single_table(stmt, from, cancel, emit)?
        {
            return Ok(Some(n));
        }
        // Build the deferred join under the regular byte budget.
        let mut budget = ByteBudget::new(self.max_query_bytes);
        let deferred = {
            let mut needed = alloc::collections::BTreeSet::new();
            let prunable = collect_qualified_refs(stmt, &mut needed).is_some();
            self.build_joined_filtered_rows(
                from,
                stmt.where_.as_ref(),
                cancel,
                if prunable { Some(&needed) } else { None },
                &mut budget,
            )?
        };
        let combined_schema = &deferred.combined_schema;
        // v7.39 (read01 round 53) — carry the catalog (see join.rs): a
        // `::regclass` / enum cast in a joined projection or HAVING needs it.
        // v7.39 (round 525) — and the session: a joined SELECT's WHERE is
        // the same predicate the unjoined shape carries.
        let joined_sess = self.dml_session();
        let ctx = EvalContext::new(combined_schema, None)
            .with_catalog(self.active_catalog())
            .with_session(&joined_sess);
        let projection =
            build_projection(&stmt.items, combined_schema, "", self.backslash_escapes)?;
        // Every projection item must be a bound qualified column —
        // anything that needs `eval_expr_with_correlated` keeps the
        // materialising path.
        let bound_pos = |e: &Expr| -> Option<usize> {
            match e {
                // v7.39 (round 822) — an UNQUALIFIED column resolves here
                // too. The `qualifier.is_some()` guard this replaces meant
                // `SELECT pad FROM big` — the commonest projection there is
                // — never reached the streaming walk: it fell out at this
                // gate and re-ran on the materialising path, after the
                // deferred join structure had already been built and paid
                // for. Measured (round 821, statement_timeout=120 over 400k
                // rows): `big.pad` and `b.pad` streamed and cancelled at
                // ~65k rows in 0.14 s, while bare `pad` ran to completion in
                // 0.80 s with the timeout never consulted. `find_column_pos`
                // has always handled the unqualified case (it falls through
                // to a by-name match), so the guard narrowed the gate for no
                // reason it recorded.
                Expr::Column(c) => eval::find_column_pos(c, &ctx),
                _ => None,
            }
        };
        let proj_decomposed: Vec<(usize, usize)> = {
            let mut out = Vec::with_capacity(projection.len());
            for p in &projection {
                let Some(abs) = bound_pos(&p.expr) else {
                    return Ok(None);
                };
                let Some(k) = deferred
                    .offsets
                    .partition_point(|&o| o <= abs)
                    .checked_sub(1)
                else {
                    return Ok(None);
                };
                out.push((k, abs - deferred.offsets[k]));
            }
            out
        };
        // Emit columns once.
        let columns: Vec<ColumnSchema> = projection
            .iter()
            // v7.39 (read01 round 54) — keep the column's enum identity through
            // the projection (it lives outside the DataType lattice), or a
            // derived table / UNION / windowed result forgets it and any outer
            // `ORDER BY <enum col>` silently sorts by the label's TEXT.
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type.clone();
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        emit(crate::StreamItem::Header(&columns))?;
        let sources_ref = &deferred.sources;
        let stride = deferred.stride;
        let survivors_ref = &deferred.survivors;
        let n_surv = if stride == 0 {
            0
        } else {
            survivors_ref.len() / stride
        };
        // Reused per-row cell-ref scratch — pushes are zero-alloc
        // after the first row.
        let null_value = Value::Null;
        let mut cell_refs: Vec<&Value> = Vec::with_capacity(projection.len());
        let mut count: usize = 0;
        for surv_i in 0..n_surv {
            if surv_i.is_multiple_of(256) {
                cancel.check()?;
            }
            let tuple = &survivors_ref[surv_i * stride..(surv_i + 1) * stride];
            cell_refs.clear();
            for &(k, col_in_src) in &proj_decomposed {
                let ri = tuple[k];
                let v: &Value = if ri == usize::MAX {
                    &null_value
                } else {
                    sources_ref[k]
                        .get(ri)
                        .and_then(|r| r.values.get(col_in_src))
                        .unwrap_or(&null_value)
                };
                cell_refs.push(v);
            }
            emit(crate::StreamItem::Row(crate::RowCells::Refs(&cell_refs)))?;
            count += 1;
        }
        Ok(Some(count))
    }

    fn exec_joined_select(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.37.x (docker-fair NOTEX attack) — short-circuit COUNT(*)
        // over a LEFT ANTI JOIN. The v7.37.27 NOT EXISTS pullup
        // rewrites `SELECT COUNT(*) FROM A WHERE NOT EXISTS (SELECT 1
        // FROM B WHERE B.k = A.k)` into
        //   SELECT COUNT(*) FROM A LEFT JOIN B ON B.k = A.k
        //   WHERE B.k IS NULL
        // The general join executor builds a hash, probes every outer
        // tuple, materialises (left_padded_with_null) for every miss,
        // then runs the aggregate over the result set. For COUNT(*) we
        // only need the count — skip the tuple materialisation. Build
        // a HashSet of B's unique join values, scan A's PK index, and
        // increment the counter on each miss. PG's Merge Anti-Join
        // does roughly this; ours becomes a simple HashSet probe.
        if let Some(out) = self.try_count_star_left_anti_join_fast(stmt, from)? {
            return Ok(out);
        }
        // v7.34.5 (mailrs prod #5) — walker-driven join + early stop.
        // When ORDER BY is on an indexed primary column, walking the
        // btree in the requested direction lets the streamer break
        // after `LIMIT + OFFSET` survivors without ever materialising
        // the rest of the join — the 80 ms `mailrs_prod_not_exists`
        // plateau is exactly this shape.
        if let Some(out) = self.try_streamed_inner_join_walk_topn(stmt, from, cancel)? {
            return Ok(out);
        }
        // v7.30.3 (mailrs round-26) — the bounded single-join path
        // first; peak memory scales with LIMIT instead of the table.
        if let Some(out) = self.try_streamed_inner_join_topn(stmt, from, cancel)? {
            return Ok(out);
        }
        // v7.17.0 Phase 3.P0-43 + P0-41 — delegate the join +
        // WHERE materialisation to the shared helper so the LATERAL
        // / UNNEST / regular-catalog paths route through one place.
        // (`build_joined_filtered_rows` carries LATERAL support as
        // of Phase 3.P0-41.) Downstream we still handle aggregate /
        // projection / ORDER BY / DISTINCT / LIMIT inline because
        // those depend on the SelectStatement's items list.
        let mut budget = ByteBudget::new(self.max_query_bytes);
        let deferred = {
            let mut needed = alloc::collections::BTreeSet::new();
            let prunable = collect_qualified_refs(stmt, &mut needed).is_some();
            self.build_joined_filtered_rows(
                from,
                stmt.where_.as_ref(),
                cancel,
                if prunable { Some(&needed) } else { None },
                &mut budget,
            )?
        };
        let combined_schema = &deferred.combined_schema;
        // v7.39 (read01 round 53) — carry the catalog (see join.rs): a
        // `::regclass` / enum cast in a joined projection or HAVING needs it.
        // v7.39 (round 525) — and the session: a joined SELECT's WHERE is
        // the same predicate the unjoined shape carries.
        let joined_sess = self.dml_session();
        let ctx = EvalContext::new(combined_schema, None)
            .with_catalog(self.active_catalog())
            .with_session(&joined_sess);
        // Aggregate path: handle GROUP BY / aggregate calls over the
        // joined+filtered rows.
        if aggregate::uses_aggregate(stmt) {
            // v7.32 (P4 borrow channel, increment 2) — borrow each
            // surviving join tuple as a RowRef::Tuple; the aggregate
            // engine reads source cells by reference (bound fast path =
            // zero clone) instead of consuming materialised combined
            // Rows. This is where the +211k materialise_tuple_vals
            // clones disappear for the join+aggregate shape.
            let refs = deferred.row_refs();
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let agg = aggregate::run(
                stmt,
                crate::join::AggRows::Refs(&refs),
                combined_schema,
                None,
                Some(&agg_correlated),
                self.parallel_runner.0.as_deref(),
                Some(self.active_catalog()),
                Some(self),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }

        let projection =
            build_projection(&stmt.items, combined_schema, "", self.backslash_escapes)?;
        // v7.39 (round 734) — a set-returning projection over a JOIN.
        // This executor's projection loop treats every item as a scalar,
        // so `SELECT unnest(ARRAY[a.id, b.g]) FROM a JOIN b …` died with
        // "function unnest(integer[]) does not exist" where PG expands
        // it. The row-set executor already carries the full SRF pipeline
        // (lockstep expansion, ORDER-BY-on-expanded-rows, the round-733
        // sharding): materialise the joined survivors and hand over. The
        // WHERE is cleared — the join already applied it, and combined
        // columns resolve identically in both executors.
        if !self.srf_target_idxs(&projection).is_empty() {
            let refs = deferred.row_refs();
            let rows: Vec<Row<'static>> = refs.iter().map(|r| r.as_row().into_owned()).collect();
            let mut s2 = stmt.clone();
            s2.where_ = None;
            let schema = combined_schema.clone();
            return self.exec_select_over_rows(&s2, rows, schema, "", cancel);
        }
        // v7.33 (P4 borrow channel, increment 3) — project directly off
        // the deferred row-index tuples instead of materialising an
        // intermediate combined Row per survivor. A bound qualified
        // column is read by reference (`RowRef::get` → `tuple_value`) and
        // cloned ONCE into the output row; the old `materialise()` (a full
        // combined Row plus a source→intermediate clone per referenced
        // cell, for every survivor) is gone. A row materialises on demand
        // only when a projection or ORDER BY expression needs the eval
        // path (subquery / function / arithmetic / unqualified column).
        // Same bind-once classification the aggregate input fast path uses
        // (`accumulate_groups`), reading the same `tuple_value` mapping the
        // differential gate already covers.
        let refs = deferred.row_refs();
        let bound_pos = |e: &Expr| -> Option<usize> {
            match e {
                Expr::Column(c) if c.qualifier.is_some() => eval::find_column_pos(c, &ctx),
                _ => None,
            }
        };
        let proj_pos: Vec<Option<usize>> = projection.iter().map(|p| bound_pos(&p.expr)).collect();
        let all_proj_bound = proj_pos.iter().all(Option::is_some);
        // v7.36 (perf — mailrs Phase 1, PROJ SPGS 8.93 → ?) —
        // pre-decompose each bound projection position into
        // `(source_k, col_in_source)` so the per-row column read
        // skips the per-cell `tuple_value` partition_point + slice
        // walk. For PROJ_25k (5 cols × 25k rows = 125k tuple_value
        // calls) that walk dominated; this version reaches into
        // `pipe.sources[k].get(tuple[k])?.values[col]` directly.
        let proj_decomposed: Vec<Option<(usize, usize)>> = proj_pos
            .iter()
            .map(|p| {
                p.and_then(|abs| {
                    let k = deferred
                        .offsets
                        .partition_point(|&o| o <= abs)
                        .checked_sub(1)?;
                    Some((k, abs - deferred.offsets[k]))
                })
            })
            .collect();
        // v7.39 (round 962) — which projection items are whole-row
        // references, and to which join source. The test is
        // `locate_column` declining the name, which is the SAME resolver
        // the evaluation path uses, so this cannot drift from it: a real
        // column carrying an alias's name resolves to a position and is
        // not reported here. The source index comes from the alias
        // prefix, the way the combined schema names its columns.
        let whole_row_src: Vec<Option<usize>> = projection
            .iter()
            .map(|p| {
                let Expr::Column(c) = &p.expr else {
                    return None;
                };
                if !matches!(eval::locate_column(c, &ctx), Ok(None)) {
                    return None;
                }
                let prefix = alloc::format!("{name}.", name = c.name);
                let abs = deferred
                    .combined_schema
                    .iter()
                    .position(|s| s.name.starts_with(&prefix))?;
                deferred
                    .offsets
                    .partition_point(|&o| o <= abs)
                    .checked_sub(1)
            })
            .collect();
        // ORDER BY (when present) still evaluates against a materialised
        // Row — keep the order-key encoder correct rather than fork it.
        let need_eval_row = !all_proj_bound || !stmt.order_by.is_empty();
        let mut tagged: Vec<(Vec<OrderKey>, Row<'static>)> = Vec::new();
        let mut proj_memo = memoize::MemoizeCache::default();
        let sources_ref = &deferred.sources;
        let stride = deferred.stride;
        let survivors_ref = &deferred.survivors;
        let n_surv = survivors_ref.len() / stride.max(1);
        // v7.38 (read01 B8) — streaming top-N budget (see the sibling
        // single-table path). Bounds this JOIN projection's accumulator
        // to O(keep) for `ORDER BY … LIMIT k`.
        let topk_stream: Option<(usize, Vec<bool>)> = if !stmt.order_by.is_empty()
            && !stmt.distinct
            && !stmt.limit_with_ties
            && !self.env_cfg().disable_topk
        {
            stmt.limit_literal().and_then(|l| {
                let keep = (l as usize).saturating_add(stmt.offset_literal().unwrap_or(0) as usize);
                (keep >= 1).then(|| (keep, stmt.order_by.iter().map(|o| o.desc).collect()))
            })
        } else {
            None
        };
        // v7.37.16 — streaming DISTINCT seen-set (see scan-path twin).
        let mut seen_distinct: hashbrown::HashMap<u64, crate::distinct::DistinctBucket> =
            hashbrown::HashMap::new();
        let distinct_hb = hashbrown::DefaultHashBuilder::default();
        for surv_i in 0..n_surv {
            let tuple = &survivors_ref[surv_i * stride..(surv_i + 1) * stride];
            let row = &refs[surv_i];
            let materialised: Option<Cow<'_, Row<'static>>> = if need_eval_row {
                Some(row.as_row())
            } else {
                None
            };
            let mut values = Vec::with_capacity(projection.len());
            for (i, p) in projection.iter().enumerate() {
                if let Some((k, col_in_src)) = proj_decomposed[i] {
                    // v7.36 — direct (source_k, col) lookup, no
                    // partition_point. tuple[k] is the row index in
                    // sources[k]; LEFT-NULL slots are `usize::MAX`.
                    let ri = tuple[k];
                    let v: Value<'static> = if ri == usize::MAX {
                        Value::Null
                    } else {
                        sources_ref[k]
                            .get(ri)
                            .and_then(|r| r.values.get(col_in_src))
                            .cloned()
                            .map(Value::into_owned)
                            .unwrap_or(Value::Null)
                    };
                    values.push(v);
                } else if let Some(pos) = proj_pos[i] {
                    // Bound but couldn't decompose (shouldn't normally
                    // happen — keep as a safe path).
                    values.push(
                        row.get(pos)
                            .cloned()
                            .map(Value::into_owned)
                            .unwrap_or(Value::Null),
                    );
                } else if let Some(k) = whole_row_src[i]
                    && tuple[k] == usize::MAX
                {
                    // v7.39 (round 962) — a whole-row reference to a side
                    // an OUTER join null-extended is NULL, not a
                    // composite whose fields are all NULL. PG18.4 answers
                    // `SELECT jb FROM wr LEFT JOIN jb ON <no match>` with
                    // an empty cell; round 961 answered `(,)`.
                    //
                    // The evaluator below cannot tell the two apart: it
                    // reads the MATERIALISED combined row, where a
                    // null-extended side is indistinguishable from a real
                    // row whose every column is NULL — and that row is
                    // `(,)` in PG too, so guessing by "all fields NULL"
                    // would trade one wrong answer for another. The
                    // tuple, which is still in hand here, does know:
                    // `usize::MAX` is the sentinel the join writes for
                    // exactly this.
                    values.push(Value::Null);
                } else {
                    // Eval path — `materialised` is Some whenever any
                    // projection item is non-bound (need_eval_row true).
                    // v7.24 (round-16 B) — select-list subqueries under a
                    // JOIN go through the correlated-aware evaluator too.
                    let mrow = materialised.as_deref().expect("materialised for eval");
                    values.push(self.eval_expr_with_correlated(
                        &p.expr,
                        mrow,
                        &ctx,
                        cancel,
                        Some(&mut proj_memo),
                    )?);
                }
            }
            let out_row = Row::new(values);
            // v7.37.16 — streaming DISTINCT (see the scan-path twin):
            // probe on the projected row; duplicates skip the
            // build_order_keys eval and never enter `tagged`.
            if stmt.distinct {
                let bucket = seen_distinct
                    .entry(norm_hash_row(&out_row, &distinct_hb, ctx.mysql_dialect))
                    .or_default();
                if bucket
                    .iter()
                    .any(|i| row_eq_norm(&tagged[i].1, &out_row, ctx.mysql_dialect))
                {
                    continue;
                }
                bucket.push(tagged.len());
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                let mrow = materialised.as_deref().expect("materialised for order by");
                build_order_keys(&stmt.order_by, mrow, &ctx)?
            };
            budget.charge(approx_row_bytes(&out_row))?;
            tagged.push((order_keys, out_row));
            if let Some((k, descs)) = &topk_stream {
                topk_trim(&mut tagged, *k, descs);
            }
        }
        if !stmt.order_by.is_empty() {
            // v7.38 元机制 D acceptor — see other call site above.
            let keep = if self.env_cfg().disable_topk {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            // v7.39 (round 688) — the join's ORDER BY resolves its keys
            // against `ctx`, which is built from `build_combined_schema`, so
            // this is where a declared collation reaches the sort. There was
            // exactly ONE resolver call in the engine before this — the
            // single-table scan's — which is why every other shape sorted by
            // bytes no matter what the schemas carried.
            let colls = crate::orderby::order_by_collations(&stmt.order_by, &ctx)?;
            crate::orderby::partial_sort_tagged_in(&mut tagged, keep, &descs, &colls);
        }
        let mut output_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
        apply_offset_and_limit(
            &mut output_rows,
            stmt.offset_literal(),
            stmt.limit_literal(),
        );
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| {
                let mut c = ColumnSchema::new(p.output_name, p.ty, p.nullable);
                c.user_enum_type = p.user_enum_type;
                c.collation_name = p.collation_name;
                c.mysql_fsp = p.mysql_fsp;
                c
            })
            .collect();
        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }
}

impl Engine {
    /// v6.10.2 — cold-tier time-travel scan. Resolves the segment
    /// by id, decodes each row body against the table's current
    /// schema, applies the SELECT's projection + optional WHERE +
    /// optional LIMIT, returns a `Rows` result. JOINs / aggregates
    /// / ORDER BY are unsupported on this path (STABILITY carve-
    /// out); operators wanting them should restore the segment
    /// into a regular table first.
    fn exec_select_as_of_segment(
        &self,
        stmt: &SelectStatement,
        from: &spg_sql::ast::FromClause,
        segment_id: u32,
    ) -> Result<QueryResult, EngineError> {
        // v6.10.2 scope: no joins, no aggregates, no ORDER BY,
        // no GROUP BY / HAVING / UNION / OFFSET / DISTINCT.
        if !from.joins.is_empty()
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.offset.is_some()
            || stmt.distinct
            || aggregate::uses_aggregate(stmt)
        {
            return Err(EngineError::Unsupported(
                "AS OF SEGMENT supports SELECT projection + WHERE + LIMIT only \
                 (joins / aggregates / ORDER BY are STABILITY § \"Out of v6.10\")"
                    .into(),
            ));
        }
        let table = self
            .active_catalog()
            .get(&from.primary.name)
            .ok_or_else(|| StorageError::TableNotFound {
                name: from.primary.name.clone(),
            })?;
        let schema = table.schema().clone();
        let schema_cols = &schema.columns;
        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let seg = self
            .active_catalog()
            .cold_segment(segment_id)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "AS OF SEGMENT: cold segment {segment_id} not registered"
                ))
            })?;
        let mut out_rows: Vec<Row<'static>> = Vec::new();
        let mut limit_remaining: Option<usize> =
            stmt.limit_literal().and_then(|n| usize::try_from(n).ok());
        for (_key, body) in seg.scan() {
            let (row, _consumed) =
                spg_storage::decode_row_body_dense(&body, &schema, seg.codec_version())
                    .map_err(EngineError::Storage)?;
            if let Some(where_expr) = &stmt.where_ {
                let cond = self.eval_expr_simple(where_expr, &row, &ctx)?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    continue;
                }
            }
            // Projection.
            let projected = self.project_row_simple(&row, &stmt.items, schema_cols, alias)?;
            out_rows.push(projected);
            if let Some(rem) = limit_remaining.as_mut() {
                if *rem == 0 {
                    out_rows.pop();
                    break;
                }
                *rem -= 1;
            }
        }
        // Output column schema: derive from SELECT items.
        let columns = self.derive_output_columns(&stmt.items, schema_cols, alias);
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }

    /// v6.10.2 — simple-path WHERE eval that doesn't go through
    /// the correlated-subquery / Memoize machinery. AS OF SEGMENT
    /// scan paths predicate against a snapshot frozen segment, no
    /// cross-row state.
    fn eval_expr_simple(
        &self,
        expr: &Expr,
        row: &Row<'static>,
        ctx: &EvalContext,
    ) -> Result<Value<'static>, EngineError> {
        let cancel = CancelToken::none();
        self.eval_expr_with_correlated(expr, row, ctx, cancel, None)
    }
}

// ---- SELECT result / projection / generate-series / SRF helpers (lib.rs split 12) ----

/// One row-producing projection: an expression to evaluate, the resulting
/// column's user-visible name, its inferred type, and nullability.
#[derive(Debug, Clone)]
pub(crate) struct ProjectedItem {
    pub(crate) expr: Expr,
    pub(crate) output_name: String,
    pub(crate) ty: DataType,
    pub(crate) nullable: bool,
    /// v7.39 (read01 round 54) — a projected enum column keeps its enum
    /// identity. Enum-ness lives outside the DataType lattice (the value is a
    /// Text), so a projection that dropped this made the RESULT schema forget
    /// it — and a UNION's combined `ORDER BY <enum col>`, which sorts against
    /// that schema, silently fell back to TEXT order instead of member order.
    pub(crate) user_enum_type: Option<String>,
    /// v7.39 (round 425) — a projected MySQL temporal column keeps its
    /// declared fractional-seconds precision, so the renderer can pad to
    /// exactly that many digits (`DATETIME(3)` shows `.250`, and `.000` for
    /// a whole second). Like `user_enum_type` this lives outside the
    /// DataType lattice, so a projection that dropped it made the RESULT
    /// schema forget how wide the fraction should print.
    pub(crate) mysql_fsp: Option<u8>,
    /// v7.39 (round 688) — and its declared collation, the third thing to
    /// live outside the DataType lattice and the third to be lost the same
    /// way. Measured: `SELECT a.loc FROM a JOIN b … ORDER BY a.loc` over a
    /// column declared `COLLATE "en_US.utf8"` sorted by bytes, because the
    /// projection rebuilt the output column and the ORDER BY resolves
    /// against THAT schema.
    pub(crate) collation_name: Option<String>,
}

/// Dedupe a row set, preserving first-seen order. `Row`'s `PartialEq` is
/// structural (`Vec<Value<'static>>` ⇒ pairwise `Value` equality), which gives SQL
/// `NULL = NULL → TRUE` and `NaN = NaN → FALSE`. The first agrees with
/// the spec's "two NULLs are not distinct"; the second is a tolerated
/// quirk for v1 (no NaN literals are reachable from the SQL surface).
/// v7.37 D.23 — is this expression a bare (non-window) aggregate call?
fn expr_is_aggregate_call(e: &Expr) -> bool {
    match e {
        Expr::FunctionCall { name, .. } => crate::aggregate::is_aggregate_name(name),
        Expr::AggregateOrdered { .. } => true,
        _ => false,
    }
}

/// Collect distinct top-level aggregate call expressions (dedup by value). Does
/// not recurse into an aggregate's own args (it's hoisted whole). Reuses the same
/// pragmatic variant set as `rewrite_window_to_columns`; aggregates nested in
/// uncovered variants simply aren't hoisted (the query keeps erroring, no worse
/// than today — never a regression on a working query).
fn collect_agg_exprs(e: &Expr, out: &mut Vec<Expr>) {
    if expr_is_aggregate_call(e) {
        if !out.iter().any(|x| x == e) {
            out.push(e.clone());
        }
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            collect_agg_exprs(lhs, out);
            collect_agg_exprs(rhs, out);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => collect_agg_exprs(expr, out),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_agg_exprs(a, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_agg_exprs(expr, out);
            collect_agg_exprs(pattern, out);
        }
        Expr::Extract { source, .. } => collect_agg_exprs(source, out),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_agg_exprs(a, out);
            }
            for p in partition_by {
                collect_agg_exprs(p, out);
            }
            for (o, _, _) in order_by {
                collect_agg_exprs(o, out);
            }
        }
        _ => {}
    }
}

/// Replace each aggregate call in `aggs` with a `Column(__aggN)` reference.
fn replace_agg_exprs(e: &mut Expr, aggs: &[Expr]) {
    if expr_is_aggregate_call(e) {
        if let Some(idx) = aggs.iter().position(|x| x == e) {
            *e = Expr::Column(ColumnName {
                qualifier: None,
                name: alloc::format!("__agg{idx}"),
            });
        }
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            replace_agg_exprs(lhs, aggs);
            replace_agg_exprs(rhs, aggs);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => replace_agg_exprs(expr, aggs),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                replace_agg_exprs(a, aggs);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            replace_agg_exprs(expr, aggs);
            replace_agg_exprs(pattern, aggs);
        }
        Expr::Extract { source, .. } => replace_agg_exprs(source, aggs),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                replace_agg_exprs(a, aggs);
            }
            for p in partition_by {
                replace_agg_exprs(p, aggs);
            }
            for (o, _, _) in order_by {
                replace_agg_exprs(o, aggs);
            }
        }
        _ => {}
    }
}

/// v7.37 D.23 — window functions run AFTER GROUP BY aggregation. Rewrite
/// `SELECT g, sum(v), rank() OVER (ORDER BY sum(v)) FROM t GROUP BY g` into an
/// aggregate derived subquery (`SELECT g, sum(v) AS __agg0 FROM t GROUP BY g`) +
/// an outer window query over it (`SELECT g, __agg0, rank() OVER (ORDER BY
/// __agg0) FROM (...) __aggwin`), which the window-over-derived path (D.13) runs.
/// Returns None outside the bounded subset (leaves current behaviour). Only fires
/// on the currently-erroring agg+window+GROUP BY shape → cannot regress working
/// window-only / aggregate-only queries.
fn rewrite_agg_before_window(stmt: &SelectStatement) -> Option<SelectStatement> {
    if !(crate::aggregate::uses_aggregate(stmt) || stmt.group_by.is_some()) {
        return None;
    }
    // Bounded subset: no set-ops; GROUP BY keys must be simple columns.
    if !stmt.unions.is_empty() {
        return None;
    }
    let group_cols: Vec<Expr> = stmt.group_by.clone().unwrap_or_default();
    if group_cols.iter().any(|g| !matches!(g, Expr::Column(_))) {
        return None;
    }
    stmt.from.as_ref()?;
    // Collect the aggregate calls to hoist from projection + outer ORDER BY.
    let mut aggs: Vec<Expr> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_agg_exprs(expr, &mut aggs);
        }
    }
    for ob in &stmt.order_by {
        collect_agg_exprs(&ob.expr, &mut aggs);
    }
    // Inner aggregate subquery: group cols (by name) + each aggregate as __aggN.
    let mut inner_items: Vec<SelectItem> = Vec::new();
    for g in &group_cols {
        inner_items.push(SelectItem::Expr {
            expr: g.clone(),
            alias: None,
        });
    }
    for (i, a) in aggs.iter().enumerate() {
        inner_items.push(SelectItem::Expr {
            expr: a.clone(),
            alias: Some(alloc::format!("__agg{i}")),
        });
    }
    let inner = SelectStatement {
        items: inner_items,
        distinct: false,
        distinct_on: Vec::new(),
        unions: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        limit_with_ties: false,
        window_check_exprs: Vec::new(),
        ..stmt.clone()
    };
    let derived = TableRef {
        name: "__aggwin".into(),
        alias: Some("__aggwin".into()),
        only: false,
        as_of_segment: None,
        unnest_expr: None,
        unnest_column_aliases: Vec::new(),
        with_ordinality: false,
        generate_series_args: None,
        lateral_subquery: Some(alloc::boxed::Box::new(inner)),
        jsonb_each_text_arg: None,
        table_fn_call: None,
        rows_from: None,
        json_table: None,
        scalar_fn_item: false,
    };
    // Outer window query over the derived rows: aggregates → __aggN column refs.
    let mut outer_items = stmt.items.clone();
    for item in &mut outer_items {
        if let SelectItem::Expr { expr, alias } = item {
            // Preserve PG's column label for a bare aggregate projection.
            if alias.is_none()
                && let Expr::FunctionCall { name, .. } = expr
                && crate::aggregate::is_aggregate_name(name)
            {
                *alias = Some(name.to_ascii_lowercase());
            }
            replace_agg_exprs(expr, &aggs);
        }
    }
    let mut outer_order = stmt.order_by.clone();
    for ob in &mut outer_order {
        replace_agg_exprs(&mut ob.expr, &aggs);
    }
    let mut outer_distinct_on = stmt.distinct_on.clone();
    for e in &mut outer_distinct_on {
        replace_agg_exprs(e, &aggs);
    }
    Some(SelectStatement {
        locking: None,
        ctes: Vec::new(),
        distinct: stmt.distinct,
        distinct_on: outer_distinct_on,
        items: outer_items,
        from: Some(FromClause {
            primary: derived,
            joins: Vec::new(),
        }),
        where_: None,
        group_by: None,
        group_by_all: false,
        having: None,
        unions: Vec::new(),
        order_by: outer_order,
        limit: stmt.limit.clone(),
        offset: stmt.offset.clone(),
        limit_with_ties: stmt.limit_with_ties,
        window_check_exprs: Vec::new(),
    })
}

/// v7.39 (round 591) — the right-hand side of a set operation, bucketed for
/// membership.
///
/// INTERSECT, EXCEPT and their ALL forms all ask "is this left row over
/// there?", and all four answered by scanning the whole right side once per
/// left row. The cost was (left rows x right rows), which is why
/// `500k INTERSECT 1000` took 1.67 s while the same two inputs the other way
/// round took 20 ms: a left row that MATCHES stops the scan early, and a left
/// row that does not pays for all of it. Over 100k left rows, raising the
/// right side from 100 to 10,000 took 35 ms to 2848.
///
/// This is the shape round 485 already solved for DISTINCT, and it reuses
/// that machinery: bucket by `norm_hash_row`, whose only guarantee is the one
/// needed here — rows `row_eq_norm` calls equal hash the same — and settle
/// every bucket with the exact comparator, so a collision costs time and
/// never an answer.
struct PeerIndex<'r> {
    bh: hashbrown::DefaultHashBuilder,
    buckets: hashbrown::HashMap<u64, Vec<usize>>,
    rows: &'r [Row<'static>],
    mysql: bool,
}

impl<'r> PeerIndex<'r> {
    fn build(rows: &'r [Row<'static>], mysql: bool) -> Self {
        // ONE hasher for the whole pass: the default builder is seeded per
        // instance, so a fresh one per row would put equal rows in different
        // buckets.
        let bh = hashbrown::DefaultHashBuilder::default();
        let mut buckets: hashbrown::HashMap<u64, Vec<usize>> =
            hashbrown::HashMap::with_capacity(rows.len());
        for (i, r) in rows.iter().enumerate() {
            buckets
                .entry(norm_hash_row(r, &bh, mysql))
                .or_default()
                .push(i);
        }
        Self {
            bh,
            buckets,
            rows,
            mysql,
        }
    }

    fn contains(&self, r: &Row<'static>) -> bool {
        let h = norm_hash_row(r, &self.bh, self.mysql);
        self.buckets
            .get(&h)
            .is_some_and(|b| b.iter().any(|&i| row_eq_norm(&self.rows[i], r, self.mysql)))
    }

    /// Remove ONE occurrence, so the multiset forms cancel row for row the
    /// way the pool they replaced did.
    fn take_one(&mut self, r: &Row<'static>) -> bool {
        let h = norm_hash_row(r, &self.bh, self.mysql);
        let Some(b) = self.buckets.get_mut(&h) else {
            return false;
        };
        let Some(pos) = b
            .iter()
            .position(|&i| row_eq_norm(&self.rows[i], r, self.mysql))
        else {
            return false;
        };
        b.swap_remove(pos);
        true
    }
}

pub(crate) fn dedup_rows(rows: Vec<Row<'static>>, mysql: bool) -> Vec<Row<'static>> {
    dedup_by_row(rows, |r| r, mysql)
}

/// v7.37.16 — hash-bucketed DISTINCT. The old `out.iter().any(row_eq_norm)`
/// was O(n·u) — `SELECT DISTINCT v` over 50 k rows with ~39 k unique values
/// ran 4 SECONDS (80 µs/row) vs PG's ~5 ms. Bucket rows by `norm_hash_row`
/// and run the exact `row_eq_norm` only within a bucket: first-occurrence
/// order is preserved, and correctness needs only the one-way guarantee
/// "row_eq_norm-Equal ⇒ equal hash" (collisions are re-checked exactly).
/// Small inputs keep the linear scan — no hasher setup for a 10-row page.
fn dedup_by_row<T>(items: Vec<T>, row_of: impl Fn(&T) -> &Row<'static>, mysql: bool) -> Vec<T> {
    if items.len() <= 32 {
        let mut out: Vec<T> = Vec::with_capacity(items.len());
        for it in items {
            if !out
                .iter()
                .any(|seen| row_eq_norm(row_of(seen), row_of(&it), mysql))
            {
                out.push(it);
            }
        }
        return out;
    }
    // ONE BuildHasher instance for the whole pass — the default builder
    // is randomly seeded PER INSTANCE, so a fresh one per row would give
    // equal rows different hashes and never dedup.
    let bh = hashbrown::DefaultHashBuilder::default();
    let mut out: Vec<T> = Vec::with_capacity(items.len().min(1024));
    let mut buckets: hashbrown::HashMap<u64, crate::distinct::DistinctBucket> =
        hashbrown::HashMap::with_capacity(items.len());
    for it in items {
        let h = norm_hash_row(row_of(&it), &bh, mysql);
        let bucket = buckets.entry(h).or_default();
        if !bucket
            .iter()
            .any(|i| row_eq_norm(row_of(&out[i]), row_of(&it), mysql))
        {
            bucket.push(out.len());
            out.push(it);
        }
    }
    out
}

/// Hash companion to [`row_eq_norm`]. Guarantees only the direction dedup
/// needs: rows that `row_eq_norm` deems Equal hash identically; DISTINCT
/// rows may collide (buckets are re-checked with the exact comparator).
///
/// Domain design mirrors `value_cmp`'s equivalence classes:
/// - The numeric family (SmallInt/Int/BigInt/Float/Numeric/NumericBig)
///   shares one domain: a value that is an integer fitting i64 hashes the
///   i64 (so `Int(1)`, `BigInt(1)`, `Float(1.0)`, `Numeric(1.00)` agree);
///   anything else hashes the f64 approximation computed by THE SAME
///   formula the value_cmp float arms use (`numeric_to_f64`), so
///   `Numeric(0.5) == Float(0.5)` agree bit-for-bit. NaN (any family)
///   hashes a constant; ±Inf hash their f64 bits; -0.0 folds into 0.0.
///   Known un-closable corner: an integer in [2^53, 2^63) can compare
///   Equal to a float via value_cmp's lossy f64 arm while hashing in the
///   exact-i64 domain — mixed int/float rows at that magnitude may miss a
///   dedup (PG itself compares int8↔float8 in the lossy float8 domain).
/// - Text and BpChar share a trailing-blank-trimmed byte domain (value_cmp
///   compares them blank-insensitively; plain Text pairs that differ only
///   in trailing blanks merely collide and are separated exactly).
/// - Families value_cmp compares exactly (Bool/Date/Time/Timestamp/…)
///   hash their fields under a distinct tag.
/// - Everything value_cmp falls back to debug-format ordering for
///   (Json, arrays, vectors, geometry, ranges, …) shares one constant
///   bucket — degrades to the exact linear scan, never wrong.
fn norm_hash_row(row: &Row<'static>, bh: &hashbrown::DefaultHashBuilder, mysql: bool) -> u64 {
    norm_hash_values(&row.values, bh, mysql)
}

/// v7.39 (round 485) — the same hash over a bare value slice, so the
/// DISTINCT probe can run against a reused buffer instead of demanding a
/// `Row` that has to be allocated first (see `values_eq_norm`).
fn norm_hash_values(
    values: &[Value<'static>],
    bh: &hashbrown::DefaultHashBuilder,
    mysql: bool,
) -> u64 {
    use core::hash::{BuildHasher, Hash, Hasher};
    let mut h = bh.build_hasher();
    for v in values {
        // v7.39 (round 410) — hash the folded key when the MySQL collation
        // deduplicates a text value, so `row_eq_norm`-equal rows (`'a'` vs
        // `'A'` vs `'a '`) share a hash bucket.
        if mysql {
            if let Some(folded) = mysql_dedup_fold(v) {
                folded.hash(&mut h);
                continue;
            }
        }
        norm_hash_value(v, &mut h);
    }
    h.finish()
}

fn norm_hash_value<H: core::hash::Hasher>(v: &Value<'static>, h: &mut H) {
    const TAG_NULL: u8 = 0;
    const TAG_BOOL: u8 = 1;
    const TAG_NUM_I64: u8 = 2;
    const TAG_NUM_F64: u8 = 3;
    const TAG_TEXT: u8 = 4;
    const TAG_DATE: u8 = 6;
    const TAG_TIME: u8 = 7;
    const TAG_TIMESTAMP: u8 = 8;
    const TAG_TIMETZ: u8 = 10;
    const TAG_UUID: u8 = 11;
    const TAG_MONEY: u8 = 12;
    const TAG_BYTES: u8 = 13;
    const TAG_INTERVAL: u8 = 14;
    const TAG_CHAR1: u8 = 15;
    const TAG_OPAQUE: u8 = 255;
    // One shared writer for the numeric family: an integer value
    // representable as i64 goes exact (round-trip probe — no_std, so no
    // f64::trunc); otherwise the f64 approximation. -0.0 round-trips
    // through 0i64, folding it into 0.0 as value_cmp requires.
    let num_f64 = |h: &mut H, x: f64| {
        if x.is_nan() {
            h.write_u8(TAG_NUM_F64);
            h.write_u64(0x7ff8_dead_beef_0001); // one bucket for every NaN
            return;
        }
        const TWO63: f64 = 9_223_372_036_854_775_808.0;
        if (-TWO63..TWO63).contains(&x) {
            #[allow(clippy::cast_possible_truncation)]
            let n = x as i64;
            #[allow(clippy::cast_precision_loss)]
            if (n as f64) == x {
                h.write_u8(TAG_NUM_I64);
                h.write_i64(n);
                return;
            }
        }
        h.write_u8(TAG_NUM_F64);
        h.write_u64(x.to_bits());
    };
    match v {
        Value::Null => h.write_u8(TAG_NULL),
        Value::Bool(b) => {
            h.write_u8(TAG_BOOL);
            h.write_u8(u8::from(*b));
        }
        Value::SmallInt(n) => {
            h.write_u8(TAG_NUM_I64);
            h.write_i64(i64::from(*n));
        }
        Value::Int(n) => {
            h.write_u8(TAG_NUM_I64);
            h.write_i64(i64::from(*n));
        }
        Value::BigInt(n) => {
            h.write_u8(TAG_NUM_I64);
            h.write_i64(*n);
        }
        Value::Float(x) => num_f64(h, *x),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => match kind {
            spg_storage::NumericKind::NaN => num_f64(h, f64::NAN),
            spg_storage::NumericKind::PosInf => num_f64(h, f64::INFINITY),
            spg_storage::NumericKind::NegInf => num_f64(h, f64::NEG_INFINITY),
            spg_storage::NumericKind::Finite => {
                // Reduce trailing fractional zeros so 1.50 and 1.5 share a
                // representation, then: exact integers fitting i64 go to the
                // i64 domain; everything else uses numeric_to_f64 — the SAME
                // formula value_cmp's Numeric↔Float arm compares with.
                let (mut s, mut sc) = (*scaled, *scale);
                while sc > 0 && s % 10 == 0 {
                    s /= 10;
                    sc -= 1;
                }
                if sc == 0 {
                    if let Ok(n) = i64::try_from(s) {
                        h.write_u8(TAG_NUM_I64);
                        h.write_i64(n);
                    } else {
                        num_f64(h, crate::orderby::numeric_to_f64(s, 0));
                    }
                } else {
                    num_f64(h, crate::orderby::numeric_to_f64(s, sc));
                }
            }
        },
        // Beyond-i128 NUMERIC compares exactly via numeric_bignum_cmp; a
        // value that also fits i128 reuses the Numeric path above so
        // Big(5) and Numeric(5) agree. A genuinely huge one can't equal
        // any i128-representable value — constant bucket is safe.
        Value::NumericBig(b) => match b.to_i128() {
            Some(s) => norm_hash_value(
                &Value::Numeric {
                    scaled: s,
                    scale: b.scale(),
                    kind: spg_storage::NumericKind::Finite,
                },
                h,
            ),
            None => h.write_u8(TAG_OPAQUE),
        },
        // value_cmp compares Text↔BpChar blank-insensitively (both sides
        // trimmed), so both hash the trimmed bytes. Text pairs differing
        // only in trailing blanks collide and are split exactly in-bucket.
        Value::Text(s) | Value::BpChar(s) => {
            h.write_u8(TAG_TEXT);
            h.write(s.trim_end_matches(' ').as_bytes());
        }
        Value::Char1(c) => {
            h.write_u8(TAG_CHAR1);
            h.write_u8(*c);
        }
        Value::Date(d) => {
            h.write_u8(TAG_DATE);
            h.write_i32(*d);
        }
        Value::Time(t) => {
            h.write_u8(TAG_TIME);
            h.write_i64(*t);
        }
        Value::Timestamp(t) => {
            h.write_u8(TAG_TIMESTAMP);
            h.write_i64(*t);
        }
        Value::TimeTz { us, offset_secs } => {
            h.write_u8(TAG_TIMETZ);
            h.write_i64(*us);
            h.write_i32(*offset_secs);
        }
        Value::Uuid(u) => {
            h.write_u8(TAG_UUID);
            h.write(u);
        }
        Value::Money(c) => {
            h.write_u8(TAG_MONEY);
            h.write_i64(*c);
        }
        Value::Bytes(b) => {
            h.write_u8(TAG_BYTES);
            h.write(b.as_ref());
        }
        Value::Interval {
            months,
            days,
            micros,
        } => {
            h.write_u8(TAG_INTERVAL);
            h.write_i32(*months);
            h.write_i32(*days);
            h.write_i64(*micros);
        }
        // v7.37.16 — REAL joined the numeric value_cmp family (widened
        // to f64, same formulas as the arms), so it hashes in the shared
        // numeric domain: Real(1.5) must agree with Float(1.5)/Int/…
        // f32→f64 is exact, so equal-under-cmp implies equal bits here.
        Value::Real(x) => num_f64(h, f64::from(*x)),
        // Json (structural equality), vector families (float rendering),
        // arrays / geometry / net / ranges / composites (debug-format
        // fallback): one constant bucket — exact linear within.
        _ => h.write_u8(TAG_OPAQUE),
    }
}

/// v7.38 (read01) — row equality for DISTINCT / UNION / INTERSECT / EXCEPT that
/// treats numerically-equal exact values as one regardless of type or scale
/// (`1 = 1.0 = 1.00`), matching PG (and GROUP BY). Uses the scale-aware
/// `orderby::value_cmp`, so `Int(1)` and `Numeric{10,1}` compare Equal; plain
/// `Row` `==` would keep them distinct.
/// v7.39 (round 410) — under the MySQL dialect a set operation / DISTINCT
/// deduplicates by the session collation (`utf8mb4_uca1400_ai_ci`, which is
/// case- and accent-insensitive and PAD SPACE): `'a'`, `'A'`, and `'a '`
/// collapse to one row, exactly as GROUP BY already folds its keys. Returns
/// the folded comparison key for a text value, None for anything else (which
/// keeps the byte-exact `value_cmp` path).
fn mysql_dedup_fold(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) | Value::BpChar(s) => {
            Some(spg_storage::mysql_ci_fold(s.trim_end_matches(' ')))
        }
        _ => None,
    }
}

/// v7.39 (round 485) — how many projected rows the single-table scan
/// builds, and how many of those the DISTINCT probe throws away again.
///
/// The round-485 profile of `SELECT DISTINCT g FROM h ORDER BY g` put
/// 21 % of all samples in malloc/free called straight from the scan
/// closure. The closure's one per-row allocation is the projected
/// `Vec<Value>`, and under DISTINCT most of those are discarded a few
/// instructions later — but "most" is a guess until it is a number, so
/// these count it. (Round 480 was spent acting on an inference about a
/// branch that turned out never to run.)
/// v7.39 (round 488) — reachability counters for round 487's projection
/// binding. The interleaved panel says round 487 costs `group_500k` 13 %,
/// and a never-called-function probe rules out code layout — so the
/// question is whether that shape reaches this code at all, which is a
/// number, not an inference.
pub static SCAN_PATH_ENTERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PROJ_DIRECT_FIRE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub static PROJ_ROW_BUILT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DISTINCT_DUP_DROPPED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub(crate) fn row_eq_norm(a: &Row<'static>, b: &Row<'static>, mysql: bool) -> bool {
    values_eq_norm(&a.values, &b.values, mysql)
}

/// v7.39 (round 485) — `row_eq_norm` over bare value slices, so the
/// DISTINCT probe can compare a reused projection buffer against a kept
/// row without building a `Row` for it.
pub(crate) fn values_eq_norm(a: &[Value<'static>], b: &[Value<'static>], mysql: bool) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            if mysql {
                if let (Some(fx), Some(fy)) = (mysql_dedup_fold(x), mysql_dedup_fold(y)) {
                    return fx == fy;
                }
            }
            crate::orderby::value_cmp(x, y) == core::cmp::Ordering::Equal
        })
}

/// Coerce a `Value` to an `f64` sort key for ORDER BY. Numbers map directly;
/// NULL sorts last (treated as `+∞`); booleans are 0.0 / 1.0; text uses lex
/// order via the byte values; vectors are not sortable.
pub(crate) fn value_to_order_key(v: &Value) -> Result<OrderKey, EngineError> {
    // v7.37.16 — TEXT rides a FULL-precision key: carry the whole string
    // so values sharing a ≥6-byte common prefix (`product_001` vs
    // `product_002`, ISO timestamps stored as text, prefixed IDs / SKUs)
    // order by their exact bytes instead of the old lossy f64 coarse key.
    // Comparison is byte-lexicographic (see `order_key_elem_cmp`), which
    // matches PG's default C / binary text collation. Every other type
    // keeps the lossless-enough `f64` fast path below.
    if let Value::Text(s) = v {
        return Ok(OrderKey::Text(s.as_ref().into()));
    }
    // v7.39 (bpchar epic) — bpchar sorts by its blank-stripped form then
    // byte order (PG bpcharcmp under C collation), so mixed-pad values of
    // the same logical string order equal.
    if let Value::BpChar(s) = v {
        return Ok(OrderKey::Text(s.trim_end_matches(' ').into()));
    }
    // v7.38 (read01 P6.24) — jsonb sorts by PG's type-aware total order, so
    // carry the parsed value and compare it structurally (see
    // `order_key_elem_cmp`). Unparseable text falls back to a Text key.
    if let Value::Json(s) = v {
        return Ok(match crate::json::parse(s) {
            Ok(jv) => OrderKey::Json(jv),
            Err(_) => OrderKey::Text(s.as_ref().into()),
        });
    }
    // v7.37 — byte-orderable types PG sorts byte-wise but that have no
    // meaningful f64 projection. bytea/uuid/macaddr sort by their raw bytes;
    // inet/cidr by `[family, addr.., bits]` (family, then address, then mask),
    // matching PG's network ordering.
    match v {
        Value::Bytes(b) => return Ok(OrderKey::Bytes(b.as_ref().to_vec())),
        // v7.38 (read01, T3.C3) — arbitrary-precision NUMERIC sorts by exact value.
        Value::NumericBig(b) => return Ok(OrderKey::BigNum((**b).clone())),
        Value::Uuid(u) => return Ok(OrderKey::Bytes(u.to_vec())),
        Value::Macaddr(m) => return Ok(OrderKey::Bytes(m.to_vec())),
        Value::Macaddr8(m) => return Ok(OrderKey::Bytes(m.to_vec())),
        Value::PgLsn(l) => return Ok(OrderKey::Bytes(l.to_be_bytes().to_vec())),
        Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
            let mut key = alloc::vec::Vec::with_capacity(18);
            key.push(*family);
            key.extend_from_slice(addr);
            key.push(*bits);
            return Ok(OrderKey::Bytes(key));
        }
        _ => {}
    }
    // v7.38 (read01, U16) — one-dimensional arrays sort element-wise, then
    // shorter-first (PG: `{1} < {1,2} < {2} < {10}`). Each element carries its
    // own OrderKey so integer arrays sort numerically; a NULL element rides to
    // the end via the +INF sentinel.
    let inf = || OrderKey::NullBig;
    let arr = match v {
        Value::IntArray(a) => Some(
            a.iter()
                .map(|o| o.map_or_else(inf, |n| OrderKey::Int(i128::from(n))))
                .collect(),
        ),
        Value::SmallIntArray(a) => Some(
            a.iter()
                .map(|o| o.map_or_else(inf, |n| OrderKey::Int(i128::from(n))))
                .collect(),
        ),
        Value::BigIntArray(a) => Some(
            a.iter()
                .map(|o| o.map_or_else(inf, |n| OrderKey::Int(i128::from(n))))
                .collect(),
        ),
        Value::BoolArray(a) => Some(
            a.iter()
                .map(|o| o.map_or_else(inf, |b| OrderKey::Int(i128::from(b))))
                .collect(),
        ),
        Value::TextArray(a) => Some(
            a.iter()
                .map(|o| o.as_ref().map_or_else(inf, |s| OrderKey::Text(s.clone())))
                .collect(),
        ),
        #[allow(clippy::cast_precision_loss)]
        Value::FloatArray(a) => Some(
            a.iter()
                .map(|o| o.map_or(OrderKey::NullBig, OrderKey::Num))
                .collect(),
        ),
        Value::NumericArray(a) => Some(
            a.iter()
                .map(|o| {
                    o.map_or_else(inf, |(m, s)| {
                        OrderKey::Num(crate::orderby::numeric_to_f64(m, s))
                    })
                })
                .collect(),
        ),
        Value::DateArray(a) => Some(
            a.iter()
                .map(|o| o.map_or_else(inf, |n| OrderKey::Int(i128::from(n))))
                .collect(),
        ),
        _ => None,
    };
    if let Some(elements) = arr {
        return Ok(OrderKey::Array(elements));
    }
    // v7.39 (read01 round 56) — a COMPOSITE sorts field by field, left to
    // right, which is exactly the lexicographic element order an Array key
    // already gives: `(2,'b') < (9,'a')` because the leading field decides.
    if let Value::Composite(fields) = v {
        let elements = fields
            .iter()
            .map(|(_, fv)| value_to_order_key(fv))
            .collect::<Result<alloc::vec::Vec<_>, _>>()?;
        return Ok(OrderKey::Array(elements));
    }
    // v7.38 (read01 U31) — the integer-valued types carry an EXACT i128 key.
    // Projecting these to f64 (the historic path) silently collapses BigInt /
    // Timestamp / Time / TimeTz / Money values past 2^53, so `ORDER BY` gave
    // the wrong order for large ids and microsecond timestamps.
    match v {
        Value::SmallInt(n) => return Ok(OrderKey::Int(i128::from(*n))),
        Value::Int(n) => return Ok(OrderKey::Int(i128::from(*n))),
        Value::BigInt(n) => return Ok(OrderKey::Int(i128::from(*n))),
        // PG TIME/TIMESTAMP/DATE/MONEY/YEAR are ordered by their underlying
        // integer (days / micros / cents / calendar year); TIMETZ by the
        // UTC-equivalent micros (local wall - offset) so the same physical
        // instant in different zones sorts equal.
        Value::Date(d) => return Ok(OrderKey::Int(i128::from(*d))),
        Value::Timestamp(t) => return Ok(OrderKey::Int(i128::from(*t))),
        Value::Time(us) => return Ok(OrderKey::Int(i128::from(*us))),
        Value::Year(y) => return Ok(OrderKey::Int(i128::from(*y))),
        Value::TimeTz { us, offset_secs } => {
            return Ok(OrderKey::Int(
                i128::from(*us) - i128::from(*offset_secs) * 1_000_000,
            ));
        }
        Value::Money(c) => return Ok(OrderKey::Int(i128::from(*c))),
        _ => {}
    }
    let num = match v {
        // Callers without NULLS FIRST/LAST context (array elements,
        // histogram sampling) put NULL last, as before.
        Value::Null => return Ok(OrderKey::NullBig),
        // v7.17.0 Phase 3.P0-38 — range ordering is not supported
        // in v7.17.0 (needs lex-then-inclusivity tiebreak).
        Value::Range { .. } => {
            return Err(EngineError::Unsupported(
                "ORDER BY of a range value is not supported in v7.17.0".into(),
            ));
        }
        // v7.17.0 Phase 3.P0-39 — hstore is not orderable.
        Value::Hstore(_) => {
            return Err(EngineError::Unsupported(
                "ORDER BY of a hstore value is not supported".into(),
            ));
        }
        // v7.17.0 Phase 3.P0-40 — 2D arrays not orderable.
        Value::IntArray2D(_) | Value::BigIntArray2D(_) | Value::TextArray2D(_) => {
            return Err(EngineError::Unsupported(
                "ORDER BY of a 2D array is not supported in v7.17.0".into(),
            ));
        }
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale, .. } => {
            // Scaled integer / 10^scale, computed via f64 for sort
            // ordering only. Precision losses here only matter for
            // ORDER BY tie-breaks well past 15 significant digits.
            // `f64::powi` lives in std; we hand-roll the loop so the
            // no_std engine crate doesn't need it.
            let mut divisor = 1.0_f64;
            for _ in 0..*scale {
                divisor *= 10.0;
            }
            (*scaled as f64) / divisor
        }
        Value::Float(x) => *x,
        // v7.37.16 — REAL sorts by its exact f64 widening (it had no
        // arm and fell through to the unsupported error).
        Value::Real(x) => f64::from(*x),
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            return Err(EngineError::Unsupported(
                "ORDER BY of a raw vector column is not meaningful — use `<->`".into(),
            ));
        }
        // v7.37 — PG orders INTERVAL by its total time, treating a month as
        // 30 days (`1 hour < 90 min < 1 day < 1 mon`). Project to total micros;
        // f64 is exact for any interval under ~285 years, and only ORDER BY
        // tie-breaks past that magnitude lose precision. Matches the
        // min/max(interval) comparator in aggregate.rs.
        #[allow(clippy::cast_precision_loss)]
        Value::Interval {
            months,
            days,
            micros,
        } => {
            let total = i128::from(*months) * 30 * 86_400_000_000
                + i128::from(*days) * 86_400_000_000
                + i128::from(*micros);
            total as f64
        }
        Value::Json(_) => {
            return Err(EngineError::Unsupported(
                "ORDER BY of a JSON value is not supported — cast the document to text first"
                    .into(),
            ));
        }
        // v7.5.0 — Value is #[non_exhaustive]; future variants need
        // an explicit ORDER BY mapping. Surface as Unsupported until
        // engine support is added.
        _ => {
            return Err(EngineError::Unsupported(
                "ORDER BY of this value type is not supported".into(),
            ));
        }
    };
    Ok(OrderKey::Num(num))
}

/// Find the schema entry that a SELECT-list `Expr::Column` refers to.
/// Mirrors `resolve_column` in `eval.rs`, but returns a proper
/// `EngineError` so the projection-build path keeps `UnknownQualifier`
/// vs `ColumnNotFound` distinct.
/// PG's name for the physical row identity. It is reserved there — no table
/// can have a column called this — which is what lets `*` skip it by name.
pub(crate) const CTID_COLUMN: &str = "ctid";

/// v7.39 (round 512) — PG's system columns, in the order they are appended.
/// All six are reserved names there, which is what lets `*` skip them and
/// lets a scan tell them from a user column without a flag.
pub(crate) const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// Is this name one of them?
pub(crate) fn is_system_column(name: &str) -> bool {
    SYSTEM_COLUMNS.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// Where the scan's appended system columns begin, if this schema carries
/// them: the trailing six, named in order. A catalog view with a column of
/// its own called `xmin` does not match, which is the point.
fn system_column_tail_start(cols: &[ColumnSchema]) -> Option<usize> {
    let start = cols.len().checked_sub(SYSTEM_COLUMNS.len())?;
    cols[start..]
        .iter()
        .zip(SYSTEM_COLUMNS)
        .all(|(c, name)| c.name.eq_ignore_ascii_case(name))
        .then_some(start)
}

/// v7.39 (round 540) — which positions `*` must skip.
///
/// The rule stays round 512's — the synthetic columns are the trailing
/// six of a relation's block, matched by POSITION so a genuine `xmin`
/// column is not lost — but a JOINED schema names its columns
/// `alias.column` and lays the peers out end to end, so a peer's six sit
/// in the MIDDLE of the whole list. Grouping by qualifier first puts the
/// "trailing six" test back on the block it was written for.
fn synthetic_system_positions(cols: &[ColumnSchema]) -> alloc::vec::Vec<bool> {
    let mut skip = alloc::vec![false; cols.len()];
    fn qualifier(n: &str) -> Option<&str> {
        n.rsplit_once('.').map(|(q, _)| q)
    }
    fn bare(n: &str) -> &str {
        n.rsplit('.').next().unwrap_or(n)
    }
    let mut i = 0;
    while i < cols.len() {
        let q = qualifier(&cols[i].name);
        let mut end = i;
        while end < cols.len() && qualifier(&cols[end].name) == q {
            end += 1;
        }
        if let Some(start) = (end - i)
            .checked_sub(SYSTEM_COLUMNS.len())
            .map(|off| i + off)
            && cols[start..end]
                .iter()
                .zip(SYSTEM_COLUMNS)
                .all(|(c, name)| bare(&c.name).eq_ignore_ascii_case(name))
        {
            for s in skip.iter_mut().take(end).skip(start) {
                *s = true;
            }
        }
        i = end;
    }
    skip
}

/// v7.39 (round 511) — does this statement name `ctid` anywhere it would be
/// read? Only then is the column materialised.
pub(crate) fn expr_references_ctid(e: &Expr) -> bool {
    let mut found = false;
    crate::expr_analysis::visit_expr_columns_and_subqueries(
        e,
        &mut |c| {
            if is_system_column(&c.name) {
                found = true;
            }
        },
        &mut |_| {},
    );
    found
}

fn references_ctid(stmt: &SelectStatement) -> bool {
    let in_expr = expr_references_ctid;
    stmt.items.iter().any(|i| match i {
        SelectItem::Expr { expr, .. } => in_expr(expr),
        _ => false,
    }) || stmt.where_.as_ref().is_some_and(in_expr)
        || stmt.order_by.iter().any(|o| in_expr(&o.expr))
        || stmt
            .group_by
            .as_ref()
            .is_some_and(|g| g.iter().any(in_expr))
        || stmt.having.as_ref().is_some_and(in_expr)
}

/// v7.39 (round 961) — the whole-row schema for `SELECT t FROM t`, which
/// is a name the projection has to TYPE before any row exists.
///
/// Evaluation has answered this since round T9 (`resolve_column` builds a
/// `Value::Composite` of every column), but the typing side below had no
/// such branch and raised `column "t" does not exist` first — so the
/// feature was unreachable through a projection. Measured against PG18.4:
/// `SELECT wr FROM wr` answers `(7,z)` there and errored here.
///
/// The type is `Jsonb` + a composite marker, which is exactly how a
/// column DECLARED as a composite type is described (`ddl.rs`, round 56):
/// the value travels as a `Value::Composite` and renders in the canonical
/// `(7,z)` form. SPG has no catalog entry for a table's implicit row type,
/// so the marker names the alias and no rehydration keys off it — the
/// value arrives already built.
fn whole_row_projection_schema(alias: &str) -> ColumnSchema {
    let mut s = ColumnSchema::new(
        alloc::string::String::from(alias),
        spg_storage::DataType::Jsonb,
        true,
    );
    s.user_composite_type = Some(alloc::string::String::from(alias));
    s
}

pub(crate) fn resolve_projection_column<'a>(
    c: &ColumnName,
    schema_cols: &'a [ColumnSchema],
    table_alias: &str,
) -> Result<Cow<'a, ColumnSchema>, EngineError> {
    if let Some(q) = &c.qualifier {
        let composite = alloc::format!("{q}.{name}", name = c.name);
        if let Some(s) = schema_cols.iter().find(|s| s.name == composite) {
            return Ok(Cow::Borrowed(s));
        }
        // Single-table case: the qualifier may equal the active alias —
        // then look for the bare column name.
        if q == table_alias
            && let Some(s) = schema_cols.iter().find(|s| s.name == c.name)
        {
            return Ok(Cow::Borrowed(s));
        }
        // For multi-table schemas the qualifier is unknown only if no
        // column bears the "<q>." prefix. For single-table, the alias
        // mismatch alone is enough.
        let prefix = alloc::format!("{q}.");
        let qualifier_known =
            q == table_alias || schema_cols.iter().any(|s| s.name.starts_with(&prefix));
        if !qualifier_known {
            return Err(EngineError::Eval(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            }));
        }
        return Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        }));
    }
    if let Some(s) = schema_cols.iter().find(|s| s.name == c.name) {
        return Ok(Cow::Borrowed(s));
    }
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = schema_cols.iter().filter(|s| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Ok(Cow::Borrowed(s)),
        (Some(_), Some(_)) => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!("column reference \"{}\" is ambiguous", c.name),
        })),
        // The whole-row reference, checked LAST so a real column carrying
        // the alias's name still wins — the same precedence
        // `resolve_column` applies on the evaluation side.
        //
        // Two schema shapes reach here. A single-table (or subquery, or
        // CTE) scan carries its alias and bare column names, so the name
        // has to equal the alias. A JOIN's combined schema carries no
        // alias at all and qualifies every column `alias.col`, so the
        // alias is identified by the prefix instead — which is exactly
        // how `whole_row_composite` picks the fields out on the
        // evaluation side. Measured: `SELECT wr FROM wr JOIN jb ON …`
        // answers `(7,z)` on PG18.4 and errored here until this arm
        // covered the joined shape too.
        _ if !table_alias.is_empty() && c.name == table_alias => {
            Ok(Cow::Owned(whole_row_projection_schema(table_alias)))
        }
        _ if table_alias.is_empty() && {
            let prefix = alloc::format!("{name}.", name = c.name);
            schema_cols.iter().any(|s| s.name.starts_with(&prefix))
        } =>
        {
            Ok(Cow::Owned(whole_row_projection_schema(&c.name)))
        }
        _ => Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        })),
    }
}

/// v7.39 (round 135) — drop the synthetic `__grp_ord_*` columns injected by the
/// parser to carry per-branch GROUPING() masks into a grouping-set query's
/// ORDER BY. They must never reach the output. No-op unless such a column is
/// present, so the common path is untouched.
/// v7.39 (round 529) — the LIMIT / OFFSET that DISTINCT ON deferred.
///
/// PG limits what the dedup LEFT, not what fed it; SPG limited first, so
/// a `LIMIT 2` that should have answered two groups answered one.
fn apply_deferred_limit(
    rows: alloc::vec::Vec<Row<'static>>,
    deferred: &(
        Option<spg_sql::ast::LimitExpr>,
        Option<spg_sql::ast::LimitExpr>,
    ),
) -> alloc::vec::Vec<Row<'static>> {
    let count = |e: &Option<spg_sql::ast::LimitExpr>| match e {
        Some(spg_sql::ast::LimitExpr::Literal(n)) => Some(*n as usize),
        _ => None,
    };
    let mut rows = rows;
    if let Some(off) = count(&deferred.1) {
        rows = rows.split_off(off.min(rows.len()));
    }
    if let Some(lim) = count(&deferred.0) {
        rows.truncate(lim);
    }
    rows
}

fn strip_synthetic_order_cols(result: QueryResult) -> QueryResult {
    let QueryResult::Rows { columns, rows } = result else {
        return result;
    };
    if !columns.iter().any(|c| c.name.starts_with("__grp_ord_")) {
        return QueryResult::Rows { columns, rows };
    }
    let keep: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.name.starts_with("__grp_ord_"))
        .map(|(i, _)| i)
        .collect();
    let new_cols: Vec<ColumnSchema> = keep.iter().map(|&i| columns[i].clone()).collect();
    let new_rows: Vec<Row<'static>> = rows
        .into_iter()
        .map(|r| Row::new(keep.iter().map(|&i| r.values[i].clone()).collect()))
        .collect();
    QueryResult::Rows {
        columns: new_cols,
        rows: new_rows,
    }
}

/// v7.39 (round 487) — bind every projection item that is a bare column
/// reference to its position, once per query.
///
/// `#[inline(never)]` and out of line on purpose. Round 486 established
/// that adding code inside these scan bodies moves neighbouring hot
/// functions around under fat LTO: the first version of this had the loop
/// inline in `run_single_table_scan` and four aggregate shapes that never
/// touch that function — `full_agg`, `join_agg`, `group_500k`,
/// `filter_agg` — went up ~5 %, reproduced against the parent commit on
/// the same machine. Keeping it out of line kept them still.
#[inline(never)]
fn bind_direct_columns(
    projection: &[ProjectedItem],
    ctx: &eval::EvalContext<'_>,
) -> Vec<Option<usize>> {
    projection
        .iter()
        .map(|p| match &p.expr {
            Expr::Column(c) => eval::compile_column_pos(c, ctx).filter(|pos| {
                // Same exclusion `compile_into` makes: a composite column
                // has to be rehydrated from stored JSON, which is not a
                // cell read.
                ctx.columns
                    .get(*pos)
                    .is_none_or(|sc| sc.user_composite_type.is_none())
            }),
            _ => None,
        })
        .collect()
}

/// v7.39 (round 505) — the name an un-aliased projected expression reports.
///
/// PG18 names a call for its function and everything else `?column?`;
/// measured with `\gdesc`. SPG used to print the parsed expression back
/// out for both dialects, so `SELECT upper(s)` reported `upper(s)` and
/// name-keyed row access found nothing under `upper`.
///
/// The MySQL half is NOT this rule and is deliberately left alone here:
/// MariaDB echoes the item's SOURCE TEXT verbatim (`a+b`, spacing and all),
/// which needs the parser to hand over spans the AST does not carry yet.
/// Until it does, a MySQL session keeps the printed form — closer to what
/// MariaDB answers than `?column?` would be.
pub(crate) fn default_output_name(expr: &Expr, mysql: bool) -> String {
    if mysql {
        return expr.to_string();
    }
    spg_sql::ast::figure_column_name(expr).unwrap_or_else(|| "?column?".to_string())
}

pub(crate) fn build_projection(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
) -> Result<Vec<ProjectedItem>, EngineError> {
    build_projection_hiding_tail(items, schema_cols, table_alias, mysql, 0)
}

/// v7.39 (round 592) — `build_projection` with the last `hidden_tail` columns
/// invisible to `*`.
///
/// The windowed-SELECT path appends a synthetic `__win_N` column per window
/// function so the rewritten projection can reference the computed values as
/// ordinary columns. `*` then expanded them too, and
/// `SELECT wr.*, row_number() OVER (ORDER BY id) FROM wr` came back with an
/// EXTRA column — the internal name's value, repeated. A wrong answer, and a
/// silent one: the row simply had one more field than the client asked for.
///
/// Hidden by POSITION rather than by name, for the reason round 512 recorded
/// about the system columns: a name test looks safe until a real column
/// happens to carry the name. These are appended last, so the count is what
/// identifies them.
pub(crate) fn build_projection_hiding_tail(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
    hidden_tail: usize,
) -> Result<Vec<ProjectedItem>, EngineError> {
    let visible = schema_cols.len().saturating_sub(hidden_tail);
    // v7.39 (round 462) — a join's combined schema qualifies every column
    // `alias.col` so the deferred-join cell lookups resolve by composite
    // name. That is an internal convention, and `*` was handing it to the
    // client: PG18 answers `SELECT * FROM a JOIN b` with the BARE names
    // (`id, g, id, h` — duplicates and all), SPG answered `a.id, a.g,
    // b.id, b.h`, so name-keyed row access found nothing. Round 128 had
    // already learned this for `q.*`; plain `*` never got the same rule.
    //
    // The signal is the schema itself, not the call site: only a combined
    // join schema arrives with no table alias AND every column qualified.
    // A single-table schema carries its alias, an empty schema has nothing
    // to strip, and a synthetic schema's names carry no dot.
    let joined_schema = table_alias.is_empty()
        && !schema_cols.is_empty()
        && schema_cols.iter().all(|c| c.name.contains('.'));
    let bare_name = |name: &str| -> String {
        if !joined_schema {
            return name.to_string();
        }
        match name.split_once('.') {
            Some((_, rest)) if !rest.is_empty() => rest.to_string(),
            _ => name.to_string(),
        }
    };
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                // v7.39 (round 511) — `*` never expands a system column, as
                // PG's does not. They join the schema only when the statement
                // asked for them, so this matters for the mixed shape
                // `SELECT *, ctid FROM t`.
                //
                // v7.39 (round 512) — by POSITION, not by name. Matching on
                // the name alone looked safe because PG reserves them, and it
                // is not: `pg_replication_slots` genuinely has a column called
                // `xmin`, and `SELECT * FROM pg_replication_slots` lost it.
                // Only the trailing six, in the order the scan appends them,
                // are the synthetic ones.
                let sys_skip = synthetic_system_positions(schema_cols);
                for (idx, col) in schema_cols.iter().enumerate() {
                    if sys_skip[idx] || idx >= visible {
                        continue;
                    }
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name: bare_name(&col.name),
                        ty: col.ty,
                        nullable: col.nullable,
                        user_enum_type: col.user_enum_type.clone(),
                        mysql_fsp: col.mysql_fsp,
                        collation_name: col.collation_name.clone(),
                    });
                }
            }
            // v7.39 (round 128) — `q.*` expands to every column belonging to
            // the qualifier `q`. Single-table schemas carry bare column names
            // reachable via `table_alias`; a join's combined schema carries
            // `alias.col` names, so a column belongs to `q` when its name has
            // the `q.` prefix. PG labels the expanded columns by their bare
            // name, so the `alias.` prefix is stripped from the output name.
            SelectItem::QualifiedWildcard(q) => {
                let prefix = alloc::format!("{q}.");
                let single_table = !table_alias.is_empty() && q == table_alias;
                let mut matched = 0usize;
                for col in &schema_cols[..visible] {
                    let belongs =
                        col.name.starts_with(&prefix) || (single_table && !col.name.contains('.'));
                    if !belongs {
                        continue;
                    }
                    matched += 1;
                    let output_name = col
                        .name
                        .strip_prefix(&prefix)
                        .unwrap_or(&col.name)
                        .to_string();
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name,
                        ty: col.ty,
                        nullable: col.nullable,
                        user_enum_type: col.user_enum_type.clone(),
                        mysql_fsp: col.mysql_fsp,
                        collation_name: col.collation_name.clone(),
                    });
                }
                if matched == 0 {
                    return Err(EngineError::Eval(EvalError::UnknownQualifier {
                        qualifier: q.clone(),
                    }));
                }
            }
            SelectItem::Expr { expr, alias } => {
                // Plain column ref keeps full schema info (real type +
                // nullability). For compound expressions try the
                // describe-side function-return-type table first
                // (e.g. `SELECT now()` → Timestamptz, `SELECT
                // concat(…)` → Text). Falls back to nullable Text
                // for shapes the describe path can't resolve.
                if let Expr::Column(c) = expr {
                    let sch = resolve_projection_column(c, schema_cols, table_alias)?;
                    let output_name = alias.clone().unwrap_or_else(|| c.name.clone());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: sch.ty,
                        nullable: sch.nullable,
                        // v7.39 (read01 round 54) — a bare enum column keeps
                        // its enum identity through the projection.
                        user_enum_type: sch.user_enum_type.clone(),
                        mysql_fsp: sch.mysql_fsp,
                        collation_name: sch.collation_name.clone(),
                    });
                } else if let Some(shape) = describe::describe_expr(expr, schema_cols) {
                    let output_name = alias
                        .clone()
                        .unwrap_or_else(|| default_output_name(expr, mysql));
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: shape.ty,
                        // v7.39 (round 258) — a projected EXPRESSION keeps its
                        // enum identity too, not just a bare column. `FROM
                        // (VALUES ('happy'::mood), …) t(m)` lowers to constant
                        // SELECTs, so the derived column arrived here as a cast
                        // and lost the enum — making the outer ORDER BY / min /
                        // max / array_agg sort by the label's TEXT.
                        nullable: shape.nullable,
                        user_enum_type: None,
                        mysql_fsp: crate::eval::expr_mysql_fsp(expr, schema_cols),
                        // A bare column reference keeps its collation; any
                        // other expression produces a new value and has none.
                        collation_name: match expr {
                            Expr::Column(c) => schema_cols
                                .iter()
                                .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                                .and_then(|sc| sc.collation_name.clone()),
                            _ => None,
                        },
                    });
                } else {
                    let output_name = alias
                        .clone()
                        .unwrap_or_else(|| default_output_name(expr, mysql));
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        // A user ENUM has no DataType of its own, so
                        // `describe_expr` cannot type `'ok'::mood` and the
                        // item lands HERE, defaulting to text — which is why
                        // pg_typeof answered `text` and a derived table sorted
                        // enum values by their label.
                        ty: DataType::Text,
                        nullable: true,
                        user_enum_type: crate::eval::expr_enum_type_name_pub(expr, schema_cols)
                            .map(alloc::string::String::from),
                        mysql_fsp: crate::eval::expr_mysql_fsp(expr, schema_cols),
                        collation_name: match expr {
                            Expr::Column(c) => schema_cols
                                .iter()
                                .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                                .and_then(|sc| sc.collation_name.clone()),
                            _ => None,
                        },
                    });
                }
            }
        }
    }
    Ok(out)
}

// ---- v4.12 window-function helpers ----
// The (partition-key, order-key, original-index) tuple shape used
// across these helpers is intrinsic to the planner. Factoring it
// into a typedef adds indirection without making the code clearer,
// so several lints are allowed inline on the affected functions
// rather than module-wide.

/// v4.22: pick more specific column types from observed rows when
/// the projection builder defaulted to Text (the v1.x behavior for
/// non-column expressions). Lets `WITH t(n) AS (SELECT 1 ...)`
/// land an Int column in the CTE storage table rather than failing
/// the insert with "expected TEXT, got INT".
pub(crate) fn infer_column_types(
    columns: &[ColumnSchema],
    rows: &[Row<'static>],
) -> Vec<ColumnSchema> {
    let mut out = columns.to_vec();
    for (col_idx, col) in out.iter_mut().enumerate() {
        if col.ty != DataType::Text {
            continue;
        }
        let mut inferred: Option<DataType> = None;
        let mut all_null = true;
        for row in rows {
            let Some(v) = row.values.get(col_idx) else {
                continue;
            };
            let ty = match v {
                Value::Null => continue,
                Value::SmallInt(_) => DataType::SmallInt,
                Value::Int(_) => DataType::Int,
                Value::BigInt(_) => DataType::BigInt,
                Value::Float(_) => DataType::Float,
                Value::Bool(_) => DataType::Bool,
                Value::Vector(_) => DataType::Vector {
                    dim: 0,
                    encoding: VecEncoding::F32,
                },
                // v7.38 (read01 U16) — carry array values through with an
                // array type so a recursive CTE that projects an array
                // (e.g. a SEARCH/CYCLE ord / path column) types the working
                // column as an array, not Text.
                Value::TextArray(_) => DataType::TextArray,
                Value::IntArray(_) => DataType::IntArray,
                Value::BigIntArray(_) => DataType::BigIntArray,
                Value::SmallIntArray(_) => DataType::SmallIntArray,
                Value::FloatArray(_) => DataType::FloatArray,
                Value::BoolArray(_) => DataType::BoolArray,
                // v7.39 (GUC knife 2) — an interval projection describes
                // as INTERVAL (typed drivers read the RowDescription OID).
                Value::Interval { .. } => DataType::Interval,
                _ => DataType::Text,
            };
            all_null = false;
            inferred = Some(match inferred {
                None => ty,
                Some(prev) if prev == ty => prev,
                Some(_) => DataType::Text,
            });
        }
        if let Some(t) = inferred {
            col.ty = t;
            col.nullable = true;
        } else if all_null {
            col.nullable = true;
        }
    }
    out
}

/// Numeric widening rank for UNION type resolution (higher = wider).
fn numeric_rank(t: DataType) -> Option<u8> {
    match t {
        DataType::SmallInt => Some(1),
        DataType::Int => Some(2),
        DataType::BigInt => Some(3),
        DataType::Numeric { .. } => Some(4),
        DataType::Float => Some(5),
        _ => None,
    }
}

/// Resolve the common result type for a UNION / VALUES column from the
/// set of concrete (non-NULL) branch types, following the safe subset
/// of PG's type resolution:
///   * all-numeric  → the widest numeric (int ∪ bigint → bigint, … ∪
///     numeric → numeric, … ∪ float → float);
///   * DATE ∪ TIMESTAMP → TIMESTAMP;
///   * exactly one concrete non-TEXT type mixed with TEXT literals →
///     that concrete type (the TEXT cells get parsed into it).
/// Returns `None` for anything ambiguous, so the caller leaves the
/// column untouched rather than risk a wrong or failing coercion.
fn resolve_union_common_type(types: &[DataType]) -> Option<DataType> {
    // NB: types are collected from RUNTIME values, which are coarser
    // than the schema (e.g. a timestamptz cell is Value::Timestamp), so
    // a single-concrete-type fast path must NOT overwrite the column
    // type — it would downgrade tstz to ts. NULL-only unification (PG:
    // `VALUES (NULL),(1.5)` types the column numeric even on the NULL
    // row's pg_typeof) needs schema-level resolution — recorded, not
    // attempted here.
    if types.len() < 2 {
        return None;
    }
    if types.iter().all(|t| numeric_rank(*t).is_some()) {
        return types
            .iter()
            .max_by_key(|t| numeric_rank(**t).unwrap_or(0))
            .copied();
    }
    let non_text: Vec<&DataType> = types
        .iter()
        .filter(|t| !matches!(t, DataType::Text))
        .collect();
    // v7.38 (T-tstz Phase 1) — temporal common type, per PG18.4: if any branch
    // is timestamptz the result is timestamptz (tstz ∪ ts, tstz ∪ date), else
    // if any is timestamp the result is timestamp (ts ∪ date). All values are
    // the same UTC-micros instant, so widening date/ts to tstz is lossless.
    if non_text.iter().all(|t| {
        matches!(
            t,
            DataType::Date | DataType::Timestamp | DataType::Timestamptz
        )
    }) && non_text
        .iter()
        .any(|t| matches!(t, DataType::Timestamp | DataType::Timestamptz))
    {
        if non_text.iter().any(|t| matches!(t, DataType::Timestamptz)) {
            return Some(DataType::Timestamptz);
        }
        return Some(DataType::Timestamp);
    }
    // A single concrete non-TEXT type mixed with TEXT literals.
    if non_text.len() == 1 {
        return Some(*non_text[0]);
    }
    // v7.37.16 — SEVERAL concrete types mixed with TEXT literals
    // (`VALUES ('NaN'::float8),(1.0),('NaN')` → float8 ∪ numeric ∪
    // text): resolve the concrete set first (PG treats the unknown-
    // typed string literals as castable to whatever the knowns
    // resolve to), then the TEXT cells parse into that target — the
    // caller's coercion dry-run still abandons the column if any
    // literal doesn't parse.
    if !non_text.is_empty() && non_text.len() < types.len() {
        let concrete: Vec<DataType> = non_text.iter().map(|t| **t).collect();
        return resolve_union_common_type(&concrete);
    }
    None
}

/// Coerce every cell of a UNION / VALUES result column to one common
/// type (see [`resolve_union_common_type`]). Conservative: a column
/// whose branches already agree, or whose types don't resolve, or where
/// any cell fails to coerce, is left exactly as it was — this never
/// turns a previously-working query into an error.
fn unify_union_columns(columns: &mut [ColumnSchema], rows: &mut [Row<'static>]) {
    for col_idx in 0..columns.len() {
        let mut seen: Vec<DataType> = Vec::new();
        for row in rows.iter() {
            if let Some(dt) = row.values.get(col_idx).and_then(Value::data_type) {
                if !seen.contains(&dt) {
                    seen.push(dt);
                }
            }
        }
        // v7.37.16 — a single concrete runtime type under a TEXT-typed
        // column means the column type came off a NULL (or unknown-text)
        // branch: NULL literals describe as TEXT (`L::Null → Text`), so
        // `VALUES (NULL),(1.5)` left the column "text" while every
        // non-NULL cell is numeric. Adopt the concrete type — schema
        // only, no cell changes. tstz-safe by construction: a real
        // timestamptz column's schema type is Timestamptz, not Text, so
        // the coarser runtime type (Value::Timestamp) can't downgrade it
        // through this arm; and a real text column's non-NULL cells are
        // Text, which keeps seen == [Text] and skips it.
        if seen.len() == 1
            && matches!(columns[col_idx].ty, DataType::Text)
            && !matches!(seen[0], DataType::Text)
        {
            columns[col_idx].ty = seen[0];
            continue;
        }
        let Some(target) = resolve_union_common_type(&seen) else {
            continue;
        };
        // v7.38 (read01) — an unconstrained NUMERIC result column keeps each
        // value's own scale in PG (`VALUES (1.0),(1.00)` renders `1.0` / `1.00`,
        // not `1.00` / `1.00`). So when the common type is NUMERIC, leave an
        // existing numeric cell untouched and only promote integers (to scale 0)
        // rather than rescaling everything to the widest scale.
        let scale_preserving_numeric = matches!(target, DataType::Numeric { .. });
        // Dry-run the coercion; abandon the whole column if any fails.
        let mut coerced: Vec<Option<Value<'static>>> = Vec::with_capacity(rows.len());
        let mut ok = true;
        for row in rows.iter() {
            match row.values.get(col_idx) {
                Some(Value::Numeric { .. }) if scale_preserving_numeric => {
                    coerced.push(Some(row.values[col_idx].clone()));
                }
                Some(v) => {
                    let cell_target = if scale_preserving_numeric {
                        DataType::Numeric {
                            precision: 0,
                            scale: 0,
                        }
                    } else {
                        target
                    };
                    match crate::conversions::coerce_value(
                        v.clone(),
                        cell_target,
                        &columns[col_idx].name,
                        col_idx,
                    ) {
                        Ok(cv) => coerced.push(Some(cv)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                None => coerced.push(None),
            }
        }
        if !ok {
            continue;
        }
        for (row, cv) in rows.iter_mut().zip(coerced) {
            if let (Some(slot), Some(nv)) = (row.values.get_mut(col_idx), cv) {
                *slot = nv;
            }
        }
        columns[col_idx].ty = target;
    }
}

/// v4.22: encode a Row to a comparable byte key for UNION-DISTINCT
/// dedup inside the recursive iteration. Crude but deterministic
/// — Debug prints embed type discriminants so NULL ≠ "" ≠ 0.
fn encode_row_key(row: &Row<'static>) -> Vec<u8> {
    let mut out = Vec::new();
    for v in &row.values {
        // v7.38 (read01) — UNION / DISTINCT dedup must treat numerically-equal
        // exact values as one, regardless of type or scale (`1 = 1.0 = 1.00`),
        // like PG (and like GROUP BY, which already normalizes). The old
        // `{v:?}` key made `Numeric{10,1}` differ from `Numeric{100,2}`. Encode
        // the exact-decimal family through one scale-stripped canonical form.
        match v {
            Value::SmallInt(n) => encode_numeric_key(&mut out, i128::from(*n), 0),
            Value::Int(n) => encode_numeric_key(&mut out, i128::from(*n), 0),
            Value::BigInt(n) => encode_numeric_key(&mut out, i128::from(*n), 0),
            Value::Numeric { scaled, scale, .. } => encode_numeric_key(&mut out, *scaled, *scale),
            other => {
                let s = alloc::format!("{other:?}|");
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

/// Append a scale-independent canonical key for an exact-decimal value: strip
/// trailing fractional zeros so `1`, `1.0`, `1.00` all key the same. The `\x01`
/// tag keeps a numeric key from colliding with a text value's `{v:?}` form.
fn encode_numeric_key(out: &mut Vec<u8>, mut scaled: i128, mut scale: u16) {
    while scale > 0 && scaled % 10 == 0 {
        scaled /= 10;
        scale -= 1;
    }
    let s = alloc::format!("\u{1}{scaled}e-{scale}|");
    out.extend_from_slice(s.as_bytes());
}

/// Multi-arg `unnest(a, b, …)` — evaluate each array argument
/// (uncorrelated; outer refs were substituted upstream), then zip
/// them in parallel, NULL-padding shorter arrays to the longest
/// (PG's ROWS FROM shorthand). Shared by the primary-position
/// executor and the join-position materialiser, which both detect
/// the parser's `__unnest_zip` marker call.
pub(crate) fn unnest_zip_rows(
    args: &[Expr],
) -> Result<(alloc::vec::Vec<DataType>, alloc::vec::Vec<Row<'static>>), EngineError> {
    let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
    let ctx = EvalContext::new(&empty_schema, None);
    let dummy_row = Row::new(alloc::vec::Vec::new());
    let mut dtypes: alloc::vec::Vec<DataType> = alloc::vec::Vec::with_capacity(args.len());
    let mut columns: alloc::vec::Vec<alloc::vec::Vec<Value<'static>>> =
        alloc::vec::Vec::with_capacity(args.len());
    for a in args {
        let v = eval::eval_expr(a, &dummy_row, &ctx).map_err(EngineError::Eval)?;
        let (dt, items): (DataType, alloc::vec::Vec<Value<'static>>) = match v {
            Value::Null => (DataType::Text, alloc::vec::Vec::new()),
            Value::TextArray(xs) => (
                DataType::Text,
                xs.into_iter()
                    .map(|x| x.map(Value::text).unwrap_or(Value::Null))
                    .collect(),
            ),
            Value::IntArray(xs) => (
                DataType::Int,
                xs.into_iter()
                    .map(|x| x.map(Value::Int).unwrap_or(Value::Null))
                    .collect(),
            ),
            Value::BigIntArray(xs) => (
                DataType::BigInt,
                xs.into_iter()
                    .map(|x| x.map(Value::BigInt).unwrap_or(Value::Null))
                    .collect(),
            ),
            other => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "unnest() expects array arguments, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                )));
            }
        };
        dtypes.push(dt);
        columns.push(items);
    }
    let max_len = columns.iter().map(|c| c.len()).max().unwrap_or(0);
    let mut rows: alloc::vec::Vec<Row<'static>> = alloc::vec::Vec::with_capacity(max_len);
    for i in 0..max_len {
        let vals: alloc::vec::Vec<Value<'static>> = columns
            .iter()
            .map(|c| c.get(i).cloned().unwrap_or(Value::Null))
            .collect();
        rows.push(Row::new(vals));
    }
    Ok((dtypes, rows))
}

/// Detect the parser's multi-arg unnest marker on an unnest_expr.
pub(crate) fn unnest_zip_args(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::FunctionCall { name, args } if name == "__unnest_zip" => Some(args.as_slice()),
        _ => None,
    }
}

/// Evaluate generate_series arguments (uncorrelated — outer refs
/// were substituted upstream where applicable) and build the row
/// stream. Dispatches on the start value's shape and rejects
/// mixed-shape calls early (e.g. start = timestamp, stop =
/// integer) so the caller gets a clean error rather than a panic.
/// Shared by the primary-position executor and the join-position
/// materialiser.
pub(crate) fn generate_series_rows(
    args: &[Expr],
    cancel: &CancelToken<'_>,
) -> Result<(DataType, alloc::vec::Vec<Row<'static>>), EngineError> {
    let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
    let ctx = EvalContext::new(&empty_schema, None);
    let dummy_row = Row::new(alloc::vec::Vec::new());
    let mut arg_values: alloc::vec::Vec<Value<'static>> =
        alloc::vec::Vec::with_capacity(args.len());
    for a in args {
        arg_values.push(eval::eval_expr(a, &dummy_row, &ctx).map_err(EngineError::Eval)?);
    }
    generate_series_from_values(arg_values, args, cancel)
}

/// v7.39 (read01 round 96) — the value-producing core of `generate_series`,
/// split out so the SELECT-list SRF path (`top_level_srf_output`) shares the
/// full integer / numeric / timestamp overload set with the FROM-clause path.
/// Before this split the target-list arm reimplemented only the integer case,
/// so `SELECT generate_series(1,2), generate_series(ts, ts, interval)` yielded
/// NULL for the timestamp column instead of the series. `arg_values` are the
/// already-evaluated arguments; `args` is kept only for the timestamptz-vs-
/// timestamp type resolution (it inspects the argument expressions' types).
pub(crate) fn generate_series_from_values(
    mut arg_values: alloc::vec::Vec<Value<'static>>,
    args: &[Expr],
    cancel: &CancelToken<'_>,
) -> Result<(DataType, alloc::vec::Vec<Row<'static>>), EngineError> {
    // PG: a NULL bound or step yields zero rows (also keeps the
    // NULL-padded lateral probe alive — schema without data).
    if arg_values.iter().any(|v| matches!(v, Value::Null)) {
        return Ok((DataType::BigInt, alloc::vec::Vec::new()));
    }
    // PG resolves `generate_series(date, date, interval)` to the
    // timestamp/timestamptz overload by implicitly casting each date
    // bound up to a timestamp at midnight (verified vs live PG18.4:
    // date args yield rows anchored at 00:00:00). SPG's TZ-naive
    // timestamp model renders the same instants, so fold any Date
    // bound to its midnight Timestamp (canonical `days *
    // 86_400_000_000`, matching cast.rs `cast_to_timestamp`) before
    // the shape match so the existing timestamp arm drives the walk.
    // v7.39 (read01 round 76) — WHICH timestamp overload PG picks matters:
    // `generate_series(date, date, interval)` has no date overload, and among
    // the two candidates PG prefers the timestamptz one (timestamptz is the
    // preferred type of the datetime category), so the column comes back
    // `timestamp with time zone` — the rows render with a `+00` offset. A
    // timestamptz bound obviously lands there too. Only genuinely
    // timestamp-typed bounds keep the TZ-naive result type.
    let empty_cols: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
    let tz = arg_values.iter().any(|v| matches!(v, Value::Date(_)))
        || args.iter().any(|a| {
            crate::describe::describe_expr(a, &empty_cols)
                .is_some_and(|s| matches!(s.ty, DataType::Timestamptz))
        });
    for v in &mut arg_values {
        if let Value::Date(d) = *v {
            *v = Value::Timestamp(crate::conversions::date_days_to_micros(d));
        }
    }
    match arg_values.as_slice() {
        [Value::Timestamp(start), Value::Timestamp(stop), step] => {
            let interval_step = match step {
                Value::Interval { .. } => step.clone(),
                // v7.38 (read01) — PG resolves an unknown-type string step
                // (`generate_series(date, date, '2 days')`) to INTERVAL; accept
                // a bare text step by parsing it the same way `::interval` does.
                Value::Text(s) => crate::conversions::coerce_value(
                    Value::text(s.as_ref()),
                    DataType::Interval,
                    "",
                    0,
                )
                .map_err(|_| {
                    EngineError::Unsupported(alloc::format!(
                        "generate_series(timestamp, timestamp, …): \
                         could not parse step {s:?} as INTERVAL"
                    ))
                })?,
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "generate_series(timestamp, timestamp, …): \
                         step must be INTERVAL, got {}",
                        crate::conversions::pg_type_name_for_error_opt(other.data_type())
                    )));
                }
            };
            let rows = generate_series_timestamps(*start, *stop, interval_step, cancel)?;
            Ok((
                if tz {
                    DataType::Timestamptz
                } else {
                    DataType::Timestamp
                },
                rows,
            ))
        }
        [start, stop, step]
            if value_is_integer(start) && value_is_integer(stop) && value_is_integer(step) =>
        {
            let s = value_to_i64(start);
            let e = value_to_i64(stop);
            let st = value_to_i64(step);
            // PG types the series by the argument type: int4 args → int4
            // elements, int8 (bigint) args → int8. Any BigInt operand widens.
            let wide = value_is_bigint(start) || value_is_bigint(stop) || value_is_bigint(step);
            let rows = generate_series_integers(s, e, st, wide, cancel)?;
            Ok((
                if wide {
                    DataType::BigInt
                } else {
                    DataType::Int
                },
                rows,
            ))
        }
        [start, stop] if value_is_integer(start) && value_is_integer(stop) => {
            let s = value_to_i64(start);
            let e = value_to_i64(stop);
            let wide = value_is_bigint(start) || value_is_bigint(stop);
            let rows = generate_series_integers(s, e, 1, wide, cancel)?;
            Ok((
                if wide {
                    DataType::BigInt
                } else {
                    DataType::Int
                },
                rows,
            ))
        }
        // v7.39 (read01 numeric.c) — the NUMERIC overload. PG walks the
        // series in exact numeric arithmetic; NaN / infinity bounds and a
        // zero step get dedicated wordings, and a mixed int/numeric call
        // resolves here via the implicit int→numeric cast.
        [_, _] | [_, _, _]
            if arg_values
                .iter()
                .any(|v| matches!(v, Value::Numeric { .. } | Value::NumericBig(_)))
                && arg_values.iter().all(|v| {
                    matches!(v, Value::Numeric { .. } | Value::NumericBig(_)) || value_is_integer(v)
                }) =>
        {
            use spg_storage::NumericKind as K;
            let words: [(&str, &str); 3] = [
                (
                    "start value cannot be NaN",
                    "start value cannot be infinity",
                ),
                ("stop value cannot be NaN", "stop value cannot be infinity"),
                ("step size cannot be NaN", "step size cannot be infinity"),
            ];
            for (i, v) in arg_values.iter().enumerate() {
                if let Value::Numeric { kind, .. } = v {
                    if *kind != K::Finite {
                        let (nan_w, inf_w) = words[i];
                        return Err(EngineError::Unsupported(
                            if *kind == K::NaN { nan_w } else { inf_w }.into(),
                        ));
                    }
                }
            }
            let big =
                |v: &Value<'_>| eval::binop::value_to_bignum(v).expect("finite numeric or integer");
            let start = big(&arg_values[0]);
            let stop = big(&arg_values[1]);
            let step = if arg_values.len() == 3 {
                big(&arg_values[2])
            } else {
                spg_storage::bignum::BigNumeric::from_i128(1, 0)
            };
            if step.is_zero() {
                return Err(EngineError::Unsupported(
                    "step size cannot equal zero".into(),
                ));
            }
            let descending = step.parts().0;
            let mut rows = alloc::vec::Vec::new();
            let mut cur = start;
            const MAX_ROWS: usize = 10_000_000;
            loop {
                cancel.check()?;
                let c = cur.cmp(&stop);
                if descending {
                    if c == core::cmp::Ordering::Less {
                        break;
                    }
                } else if c == core::cmp::Ordering::Greater {
                    break;
                }
                if rows.len() >= MAX_ROWS {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "generate_series() result exceeds {MAX_ROWS} rows"
                    )));
                }
                rows.push(Row::new(alloc::vec![eval::binop::bignum_to_value(
                    cur.clone()
                )]));
                cur = cur.add(&step);
            }
            Ok((
                DataType::Numeric {
                    precision: 0,
                    scale: 0,
                },
                rows,
            ))
        }
        _ => Err(EngineError::Unsupported(alloc::format!(
            "generate_series(): v7.17 supports integer or (timestamp, timestamp, interval) \
             argument shapes; got {}",
            arg_values
                .iter()
                .map(|v| crate::conversions::pg_type_name_for_error_opt(v.data_type()))
                .collect::<alloc::vec::Vec<_>>()
                .join(", ")
        ))),
    }
}

/// v7.17.0 Phase 3.10 — integer-mode generate_series materialiser.
/// Step direction follows the sign: positive step iterates upward
/// (stops when current > stop); negative iterates downward; zero
/// errors. Caller-facing row stream is `BigInt`-typed so a single
/// projection schema covers SmallInt / Int / BigInt callers.
fn generate_series_integers(
    start: i64,
    stop: i64,
    step: i64,
    wide: bool,
    cancel: &CancelToken<'_>,
) -> Result<alloc::vec::Vec<Row<'static>>, EngineError> {
    if step == 0 {
        return Err(EngineError::Unsupported(
            "step size cannot equal zero".into(),
        ));
    }
    let mut out = alloc::vec::Vec::new();
    let mut cur = start;
    // Hard cap to keep a runaway call from eating all memory. PG
    // has no such cap but does honour query timeout; SPG's cancel
    // token will fire too — this is a defense-in-depth backstop.
    const MAX_ROWS: usize = 10_000_000;
    loop {
        cancel.check()?;
        if step > 0 && cur > stop {
            break;
        }
        if step < 0 && cur < stop {
            break;
        }
        out.push(Row::new(alloc::vec![if wide {
            Value::BigInt(cur)
        } else {
            Value::Int(cur as i32)
        }]));
        if out.len() > MAX_ROWS {
            return Err(EngineError::Unsupported(alloc::format!(
                "generate_series(): exceeded {MAX_ROWS} rows; \
                 narrow start/stop or use a larger step"
            )));
        }
        cur = match cur.checked_add(step) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(out)
}

/// v7.17.0 Phase 3.10 — timestamp-mode generate_series. step is a
/// `Value::Interval { months, micros }` per the caller's guard;
/// each iteration adds the interval via `apply_binary_interval`
/// so month-shifting handles short-month rollover (PG semantics).
fn generate_series_timestamps(
    start: i64,
    stop: i64,
    step: Value,
    cancel: &CancelToken<'_>,
) -> Result<alloc::vec::Vec<Row<'static>>, EngineError> {
    let (months, days, micros) = match &step {
        Value::Interval {
            months,
            days,
            micros,
        } => (*months, *days, *micros),
        _ => unreachable!("caller guards step.is_interval"),
    };
    if months == 0 && days == 0 && micros == 0 {
        return Err(EngineError::Unsupported(
            "generate_series(): INTERVAL step cannot be zero".into(),
        ));
    }
    let ascending = months > 0 || days > 0 || micros > 0;
    let mut out = alloc::vec::Vec::new();
    let mut cur = Value::Timestamp(start);
    const MAX_ROWS: usize = 10_000_000;
    loop {
        cancel.check()?;
        let cur_t = match cur {
            Value::Timestamp(t) => t,
            _ => unreachable!("loop invariant: cur is Timestamp"),
        };
        if ascending && cur_t > stop {
            break;
        }
        if !ascending && cur_t < stop {
            break;
        }
        out.push(Row::new(alloc::vec![Value::Timestamp(cur_t)]));
        if out.len() > MAX_ROWS {
            return Err(EngineError::Unsupported(alloc::format!(
                "generate_series(): exceeded {MAX_ROWS} rows; \
                 narrow start/stop or use a larger step"
            )));
        }
        let next = eval::apply_binary_interval(
            spg_sql::ast::BinOp::Add,
            &cur,
            &Value::Interval {
                months,
                days,
                micros,
            },
        )
        .map_err(EngineError::Eval)?;
        cur = match next {
            Some(v) => v,
            None => break,
        };
    }
    Ok(out)
}

/// v7.17.0 Phase 3.P0-49 — PG-canonical: `FETCH FIRST <n> ROWS
/// WITH TIES` requires an `ORDER BY`. Without one, there's no
/// way to identify "ties" deterministically, so PG errors at
/// plan time. SPG mirrors that surface so the same DDL / app
/// behaviour holds on cutover.
fn check_with_ties_requires_order_by(stmt: &SelectStatement) -> Result<(), EngineError> {
    if stmt.limit_with_ties && stmt.order_by.is_empty() {
        return Err(EngineError::Unsupported(alloc::string::String::from(
            "WITH TIES cannot be specified without ORDER BY clause",
        )));
    }
    Ok(())
}

/// v7.19 P5 — true iff `expr` is `unnest(arg)` at the top level
/// (case-insensitive). Used by `exec_select_cancel`'s
/// projection loop to detect Set-Returning-Function rows that
/// need per-row expansion. Only the top-level call counts —
/// `coalesce(unnest(arr), 'x')` is NOT a SRF row from the
/// projection's perspective; it would surface as an "unknown
/// function" mismatch downstream, which is what we want
/// (multi-SRF / nested SRF is documented carve-out for v7.19).
fn is_top_level_unnest(expr: &spg_sql::ast::Expr) -> bool {
    top_level_srf_kind(expr).is_some()
}

/// v7.38 (read01, T15) — which set-returning function a top-level SELECT-list
/// call is, if any. Matching is allocation-free (`eq_ignore_ascii_case`, no
/// `to_ascii_lowercase`) because `top_level_srf_output` classifies once per
/// source row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SrfKind {
    Unnest,
    /// v7.39 (read01 round 67) — `generate_series(a, b[, step])` in the target
    /// list. It used to be handled ONLY by the parser's lift into FROM, so a
    /// second one in the same list came back as "unknown function".
    GenerateSeries,
    GenerateSubscripts,
    /// `_text` variants unwrap scalars to their lexeme; the plain forms render
    /// every value as compact JSON text.
    ArrayElements {
        as_text: bool,
    },
    PathQuery,
    RegexpMatches,
    Each {
        as_text: bool,
    },
    ObjectKeys,
}

/// Case-insensitive match against any of `names`.
fn name_is(name: &str, names: &[&str]) -> bool {
    names.iter().any(|n| name.eq_ignore_ascii_case(n))
}

pub(crate) fn top_level_srf_kind(expr: &spg_sql::ast::Expr) -> Option<SrfKind> {
    let spg_sql::ast::Expr::FunctionCall { name, args } = expr else {
        return None;
    };
    let n = args.len();
    // v7.38 (read01) — generate_subscripts(arr, dim) is set-returning in the
    // SELECT list (it returned an array there before) and shares the unnest
    // expansion machinery.
    if n == 1 && name.eq_ignore_ascii_case("unnest") {
        return Some(SrfKind::Unnest);
    }
    if (2..=3).contains(&n) && name.eq_ignore_ascii_case("generate_series") {
        return Some(SrfKind::GenerateSeries);
    }
    if n == 2 && name.eq_ignore_ascii_case("generate_subscripts") {
        return Some(SrfKind::GenerateSubscripts);
    }
    // v7.38 (read01, T15) — the jsonb/json SRF family and regexp_matches expand
    // per element / match in the SELECT list; they collapsed to a single row
    // (a TextArray, or an "unknown function" error for `each`) before.
    if n == 1 && name_is(name, &["jsonb_array_elements", "json_array_elements"]) {
        return Some(SrfKind::ArrayElements { as_text: false });
    }
    if n == 1
        && name_is(
            name,
            &["jsonb_array_elements_text", "json_array_elements_text"],
        )
    {
        return Some(SrfKind::ArrayElements { as_text: true });
    }
    // v7.39 (jsonpath depth) — 3rd arg = vars, 4th = silent.
    if (2..=4).contains(&n) && name_is(name, &["jsonb_path_query", "json_path_query"]) {
        return Some(SrfKind::PathQuery);
    }
    if (2..=3).contains(&n) && name.eq_ignore_ascii_case("regexp_matches") {
        return Some(SrfKind::RegexpMatches);
    }
    if n == 1 && name_is(name, &["jsonb_each", "json_each"]) {
        return Some(SrfKind::Each { as_text: false });
    }
    if n == 1 && name_is(name, &["jsonb_each_text", "json_each_text"]) {
        return Some(SrfKind::Each { as_text: true });
    }
    if n == 1 && name_is(name, &["jsonb_object_keys", "json_object_keys"]) {
        return Some(SrfKind::ObjectKeys);
    }
    None
}

/// v7.38 (read01) — the row-set a top-level SELECT-list SRF emits: the elements
/// for `unnest(arr)`, or the 1-based subscripts `1..=length` for
/// `generate_subscripts(arr, 1)` (a non-1 dimension over a 1-D array yields no
/// rows, as in PG).
pub(crate) fn top_level_srf_output(
    expr: &spg_sql::ast::Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Value<'static>>, EngineError> {
    let (Some(kind), spg_sql::ast::Expr::FunctionCall { name, args }) =
        (top_level_srf_kind(expr), expr)
    else {
        return Err(EngineError::Unsupported(
            "expected a SELECT-list SRF call".into(),
        ));
    };
    match kind {
        SrfKind::Unnest => {
            // v7.39 (round 743) — `unnest(ARRAY[e1, …, ek])` evaluates
            // the elements DIRECTLY: the old path built the whole
            // Value::Array (one eval + a clone per element) only for
            // array_value_to_elements to clone every element back out.
            // Any other argument shape (a column, a function result)
            // keeps the build-then-split path.
            if let spg_sql::ast::Expr::Array(items) = &args[0] {
                return items
                    .iter()
                    .map(|e| eval::eval_expr(e, row, ctx).map_err(EngineError::Eval))
                    .collect();
            }
            let arr = eval::eval_expr(&args[0], row, ctx).map_err(EngineError::Eval)?;
            array_value_to_elements(&arr)
        }
        SrfKind::GenerateSeries => {
            // v7.39 (read01 round 96) — evaluate the args against the actual
            // row, then hand off to the shared core so the numeric and
            // timestamp/timestamptz overloads work here too (this arm used to
            // handle only integers, silently NULLing a temporal/numeric series
            // when it shared a target list with another SRF).
            let mut arg_values: Vec<Value<'static>> = Vec::with_capacity(args.len());
            for a in args {
                arg_values.push(eval::eval_expr(a, row, ctx).map_err(EngineError::Eval)?);
            }
            let (_, rows) = generate_series_from_values(arg_values, args, &CancelToken::none())?;
            Ok(rows
                .into_iter()
                .map(|r| r.values.into_iter().next().unwrap_or(Value::Null))
                .collect())
        }
        SrfKind::GenerateSubscripts => {
            let arr = eval::eval_expr(&args[0], row, ctx).map_err(EngineError::Eval)?;
            let dim = eval::eval_expr(&args[1], row, ctx).map_err(EngineError::Eval)?;
            if !matches!(dim, Value::Int(1) | Value::BigInt(1) | Value::SmallInt(1)) {
                return Ok(Vec::new());
            }
            let len = array_value_to_elements(&arr)?.len();
            Ok((1..=len).map(|i| Value::Int(i as i32)).collect())
        }
        // One Value per array element (`_text` → text / SQL NULL, plain → the
        // element's compact JSON text) — the element list the FROM-clause form
        // materialises.
        SrfKind::ArrayElements { as_text } => {
            let arg = eval::eval_expr(&args[0], row, ctx).map_err(EngineError::Eval)?;
            if matches!(arg, Value::Null) {
                return Ok(Vec::new());
            }
            let items =
                crate::json::array_element_rows(&arg, as_text, name).map_err(EngineError::Eval)?;
            Ok(items
                .into_iter()
                .map(|opt| opt.map(Value::text).unwrap_or(Value::Null))
                .collect())
        }
        // The scalar form already yields a TextArray of the keys (or errors on
        // a non-object, like PG); expand it into rows.
        SrfKind::ObjectKeys => {
            let v = eval::eval_expr(expr, row, ctx).map_err(EngineError::Eval)?;
            array_value_to_elements(&v)
        }
        // One row per match, each a text[] of the pattern's capture groups.
        SrfKind::RegexpMatches => {
            let vals: Vec<Value<'static>> = args
                .iter()
                .map(|a| eval::eval_expr(a, row, ctx).map_err(EngineError::Eval))
                .collect::<Result<_, _>>()?;
            crate::eval::regexp_matches_rows(&vals).map_err(EngineError::Eval)
        }
        // One composite `(key, value)` row per object member (plain → jsonb
        // value, `_text` → text / SQL NULL).
        SrfKind::Each { as_text } => {
            let arg = eval::eval_expr(&args[0], row, ctx).map_err(EngineError::Eval)?;
            if matches!(arg, Value::Null) {
                return Ok(Vec::new());
            }
            let pairs = crate::json::each_rows(&arg, as_text, name).map_err(EngineError::Eval)?;
            Ok(pairs
                .into_iter()
                .map(|(k, v)| {
                    let val = if as_text {
                        v.map(Value::text).unwrap_or(Value::Null)
                    } else {
                        v.map(Value::json).unwrap_or(Value::Null)
                    };
                    Value::Composite(alloc::vec![
                        ("key".to_string(), Value::text(k)),
                        ("value".to_string(), val),
                    ])
                })
                .collect())
        }
        // One Value per matched JSON value.
        SrfKind::PathQuery => {
            let doc = eval::eval_expr(&args[0], row, ctx).map_err(EngineError::Eval)?;
            let path = eval::eval_expr(&args[1], row, ctx).map_err(EngineError::Eval)?;
            // v7.39 — optional vars document (3rd arg).
            let vars = match args.get(2) {
                Some(a) => {
                    let v = eval::eval_expr(a, row, ctx).map_err(EngineError::Eval)?;
                    crate::json::parse_path_vars(&v).map_err(EngineError::Eval)?
                }
                None => None,
            };
            match crate::json::path_query_vars(&doc, &path, vars.as_ref())
                .map_err(EngineError::Eval)?
            {
                Value::Null => Ok(Vec::new()),
                Value::TextArray(items) => Ok(items
                    .into_iter()
                    .map(|opt| opt.map(Value::text).unwrap_or(Value::Null))
                    .collect()),
                other => Ok(alloc::vec![other]),
            }
        }
    }
}

/// v7.19 P5 — turn an array-typed `Value` into the element list
/// `unnest()` projection emits. NULL → empty list (PG: `unnest(NULL)
/// = (no rows)`). Non-array values fall through to a type-mismatch
/// error.
pub(crate) fn array_value_to_elements(v: &Value) -> Result<Vec<Value<'static>>, EngineError> {
    // v7.39 (round 236) — PG unnests a multidimensional array into its
    // elements in row-major order (`unnest(ARRAY[[1,2],[3,4]])` is four
    // rows). SPG stores 2-D arrays as their own variants, which fell
    // through to the type-mismatch arm below.
    if let Some(flat) = crate::eval::values::flatten_2d(v) {
        return array_value_to_elements(&flat);
    }
    match v {
        Value::Null => Ok(Vec::new()),
        Value::TextArray(items) => Ok(items
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|s| Value::text(s.clone()))
                    .unwrap_or(Value::Null)
            })
            .collect()),
        Value::IntArray(items) => Ok(items
            .iter()
            .map(|opt| opt.map(Value::Int).unwrap_or(Value::Null))
            .collect()),
        Value::BigIntArray(items) => Ok(items
            .iter()
            .map(|opt| opt.map(Value::BigInt).unwrap_or(Value::Null))
            .collect()),
        // v7.39 (read01 multirangetypes.c) — unnest(anymultirange): one
        // range per canonical span.
        Value::Multirange { kind, ranges } => Ok(ranges
            .iter()
            .map(|s| Value::Range {
                kind: *kind,
                lower: s.lower.clone(),
                upper: s.upper.clone(),
                lower_inc: s.lower_inc,
                upper_inc: s.upper_inc,
                empty: false,
            })
            .collect()),
        other => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!(
                "unnest() expects an array argument, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ),
        })),
    }
}

impl Engine {
    /// v7.17.0 Phase 1.2 — find every catalog VIEW referenced in
    /// the SELECT's FROM / JOIN graph, re-parse each view's body
    /// source, and prepend it as a synthetic CTE on the
    /// returned SelectStatement. Returns `None` when no view
    /// references are found (caller proceeds with the original
    /// statement); returns `Some(rewritten)` otherwise (caller
    /// re-runs exec_select_cancel on the rewritten form so the
    /// regular CTE materialiser handles it).
    fn expand_views_in_select(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        let cat = self.active_catalog();
        let mut referenced: Vec<String> = Vec::new();
        if let Some(from) = &stmt.from {
            collect_view_refs(&from.primary, cat, &mut referenced);
            for j in &from.joins {
                collect_view_refs(&j.table, cat, &mut referenced);
            }
        }
        // Don't expand a view name that's already shadowed by a
        // CTE on the same SELECT — the CTE wins per PG.
        referenced.retain(|n| !stmt.ctes.iter().any(|c| c.name == *n));
        if referenced.is_empty() {
            return Ok(None);
        }
        let mut new_ctes: Vec<spg_sql::ast::Cte> = Vec::with_capacity(referenced.len());
        for name in &referenced {
            let view = cat.view(name).ok_or_else(|| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                    "view {name:?} disappeared mid-expansion"
                )))
            })?;
            let parsed = spg_sql::parser::parse_statement(&view.body).map_err(|e| {
                EngineError::Unsupported(alloc::format!("view {name:?} body re-parse failed: {e}"))
            })?;
            let Statement::Select(body) = parsed else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "view {name:?} body is not a SELECT (catalog corruption)"
                )));
            };
            new_ctes.push(spg_sql::ast::Cte {
                name: name.clone(),
                body: spg_sql::ast::CteBody::Select(body),
                recursive: false,
                column_overrides: view.columns.clone(),
                search: None,
                cycle: None,
            });
        }
        let mut out = stmt.clone();
        // Prepend so view CTEs are visible to caller-supplied CTEs.
        new_ctes.extend(out.ctes);
        out.ctes = new_ctes;
        Ok(Some(out))
    }

    /// v7.37.6-B(sentori Epic 2 P0)— if `stmt`'s FROM-clause references
    /// any partition-parent table, rewrite the SELECT so each parent
    /// reference resolves to a CTE whose body is a `UNION ALL` over the
    /// children that pass the WHERE-derived partition-key range. Returns
    /// `None`(no rewrite needed)when no parent is referenced or all
    /// references are shadowed by a same-name CTE.
    ///
    /// Pruning vocabulary at v7.37.6-B:
    ///   * Flat `AND` chain over `<key> {>= | > | < | <= | =} literal`
    ///     and `<key> BETWEEN literal AND literal`.
    ///   * Anything outside that(OR / nested IN / function call on the
    ///     key)defaults to "no pruning" — every child + DEFAULT lands
    ///     in the UNION. Correctness is preserved; only the plan size
    ///     widens.
    fn expand_partition_parents_in_select(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        let cat = self.active_catalog();
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        let mut parent_refs: Vec<String> = Vec::new();
        collect_partition_parent_refs(&from.primary, cat, &mut parent_refs);
        for j in &from.joins {
            collect_partition_parent_refs(&j.table, cat, &mut parent_refs);
        }
        // Drop names shadowed by a CTE on the same SELECT(PG semantics
        // — same as view expansion above).
        parent_refs.retain(|n| !stmt.ctes.iter().any(|c| c.name.eq_ignore_ascii_case(n)));
        if parent_refs.is_empty() {
            return Ok(None);
        }
        // Synthesise a CTE name per parent so the existing
        // "CTE shadows a real table" guard doesn't fire (the parent
        // IS a real table in the catalog, unlike VIEW expansion's
        // case). The FROM-clause TableRef walker below rewrites
        // every parent reference to point at the synthetic CTE.
        let synth_name = |p: &str| alloc::format!("__spg_partition_{p}");
        let mut new_ctes: Vec<spg_sql::ast::Cte> = Vec::with_capacity(parent_refs.len());
        let mut expanded_parents: Vec<alloc::string::String> = Vec::new();
        for parent_name in &parent_refs {
            // No children = no rewrite. The parent itself is a real
            // (empty-rows) table — the regular FROM-resolution path
            // will scan it and return 0 rows, matching the
            // "partition parent with no children" plan. Skipping the
            // CTE here also avoids `SELECT * FROM parent` re-entering
            // this rewrite on the synthetic body (infinite recursion).
            let Some(body) = self.build_partition_parent_union_body(parent_name, stmt)? else {
                continue;
            };
            new_ctes.push(spg_sql::ast::Cte {
                name: synth_name(parent_name),
                body: spg_sql::ast::CteBody::Select(body),
                recursive: false,
                column_overrides: Vec::new(),
                search: None,
                cycle: None,
            });
            expanded_parents.push(parent_name.clone());
        }
        if expanded_parents.is_empty() {
            return Ok(None);
        }
        let mut out = stmt.clone();
        if let Some(from) = out.from.as_mut() {
            rewrite_partition_parent_table_ref(&mut from.primary, &expanded_parents, &synth_name);
            for j in &mut from.joins {
                rewrite_partition_parent_table_ref(&mut j.table, &expanded_parents, &synth_name);
            }
        }
        new_ctes.extend(out.ctes);
        out.ctes = new_ctes;
        Ok(Some(out))
    }

    /// Build the `SELECT * FROM child1 UNION ALL …` body for one parent.
    /// Children include every overlap-hit `Range` plus(always)the
    /// `Default` child(if any). Returns `Ok(None)` when no children
    /// would survive — caller skips the CTE injection and lets the
    /// parent fall through to the regular(empty-rows)scan path,
    /// avoiding the infinite recursion that an empty-body CTE
    /// referencing the parent name would trigger.
    /// v7.37.16 (16.10) — public helper invoked from explain.rs to
    /// surface "which children survive the WHERE-clause prune" in
    /// EXPLAIN output. Returns `None` when `parent_name` isn't
    /// actually a partition parent; otherwise returns the list of
    /// children the planner would scan (same algorithm as
    /// [`Self::build_partition_parent_union_body`] but without the
    /// SQL re-parse).
    /// v7.39 (round 224) — the kept-children prune keyed off a bare WHERE
    /// expression (the PG-shaped EXPLAIN's scan builder has no full
    /// SelectStatement in hand). Wraps the original by synthesising a
    /// minimal statement carrying just the predicate.
    pub(crate) fn explain_partition_kept_children_by_where(
        &self,
        parent_name: &str,
        where_: Option<&spg_sql::ast::Expr>,
    ) -> Option<Vec<alloc::string::String>> {
        let mut synth = SelectStatement::default();
        synth.where_ = where_.cloned();
        self.explain_partition_kept_children(parent_name, &synth)
    }

    pub(crate) fn explain_partition_kept_children(
        &self,
        parent_name: &str,
        outer: &SelectStatement,
    ) -> Option<Vec<alloc::string::String>> {
        use spg_storage::PartitionRole;
        let cat = self.active_catalog();
        let parent = cat.get(parent_name)?;
        let (key_position, parent_kind) = match &parent.schema().partition_role {
            Some(PartitionRole::Parent {
                key_column_positions,
                kind,
                ..
            }) => (*key_column_positions.first().unwrap_or(&0), *kind),
            _ => return None,
        };
        let key_col_name = parent.schema().columns[key_position].name.clone();
        let (lo_bound, hi_bound) = match outer.where_.as_ref() {
            Some(expr) => extract_key_range(expr, &key_col_name),
            None => (None, None),
        };
        let eq_value: Option<spg_storage::Value<'static>> = match outer.where_.as_ref() {
            Some(expr) => extract_key_eq_value(expr, &key_col_name),
            None => None,
        };
        let children = crate::partition::children_of_parent(cat, parent_name);
        let mut kept: Vec<alloc::string::String> = Vec::new();
        let mut default_child: Option<alloc::string::String> = None;
        for child_name in &children {
            let Some(child) = cat.get(child_name) else {
                continue;
            };
            match &child.schema().partition_role {
                Some(PartitionRole::Range { lower, upper, .. }) => {
                    if range_satisfies_filter(lower, upper, lo_bound.as_ref(), hi_bound.as_ref()) {
                        kept.push(child_name.clone());
                    }
                }
                Some(PartitionRole::List { values, .. }) => match &eq_value {
                    Some(v) => {
                        if values.iter().any(|b| b.equals_value(v)) {
                            kept.push(child_name.clone());
                        }
                    }
                    None => kept.push(child_name.clone()),
                },
                Some(PartitionRole::Hash {
                    modulus, remainder, ..
                }) => match &eq_value {
                    Some(v) => {
                        let h = crate::partition::pg_compatible_hash(v);
                        if h.rem_euclid(u64::from(*modulus)) == u64::from(*remainder) {
                            kept.push(child_name.clone());
                        }
                    }
                    None => kept.push(child_name.clone()),
                },
                Some(PartitionRole::Default { .. }) => {
                    default_child = Some(child_name.clone());
                }
                _ => {}
            }
        }
        let _ = parent_kind;
        if let Some(d) = default_child {
            if kept.is_empty() || eq_value.is_none() {
                kept.push(d);
            }
        }
        Some(kept)
    }

    fn build_partition_parent_union_body(
        &self,
        parent_name: &str,
        outer: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        use spg_storage::PartitionRole;
        let cat = self.active_catalog();
        let parent = cat.get(parent_name).ok_or_else(|| {
            EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                "partition parent {parent_name:?} disappeared mid-expansion"
            )))
        })?;
        let (key_position, parent_kind) = match &parent.schema().partition_role {
            Some(PartitionRole::Parent {
                key_column_positions,
                kind,
                ..
            }) => (*key_column_positions.first().unwrap_or(&0), *kind),
            // v7.39 (round 645) — an INHERITANCE parent, which has no
            // role of its own: the relationship is recorded only in the
            // children. Three things differ from a partition parent and
            // all three are in this body.
            //
            //   * The parent HOLDS ROWS, so it is a term of the union —
            //     `FROM ONLY`, or expanding it would recurse.
            //   * There is no partition key, so there is nothing to
            //     prune: every child is a term.
            //   * A child may declare columns of its own, so the terms
            //     name the PARENT's columns rather than `*`. PG's
            //     `SELECT * FROM parent` returns the parent's shape.
            //
            // Answered from this match rather than a branch before it —
            // round 644 measured what an extra early return beside an
            // existing test costs in this file.
            _ if crate::partition::has_inheritance_children(cat, parent_name) => {
                let cols = parent
                    .schema()
                    .columns
                    .iter()
                    .map(|c| quote_ident_for_sql(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let carry_sys = references_ctid(outer);
                let sys = if carry_sys {
                    let mut t = alloc::string::String::new();
                    for s in SYSTEM_COLUMNS {
                        t.push_str(", ");
                        t.push_str(s);
                    }
                    t
                } else {
                    alloc::string::String::new()
                };
                let mut body = alloc::format!(
                    "SELECT {cols}{sys} FROM ONLY {}",
                    quote_ident_for_sql(parent_name)
                );
                for child in crate::partition::children_of_parent(cat, parent_name) {
                    body.push_str(&alloc::format!(
                        " UNION ALL SELECT {cols}{sys} FROM {}",
                        quote_ident_for_sql(&child)
                    ));
                }
                return parse_select_or_corrupt(&body).map(Some);
            }
            _ => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "partition expansion: {parent_name:?} is not a parent"
                )));
            }
        };
        let key_col_name = parent.schema().columns[key_position].name.clone();
        // v7.37.16 (16.7) — for RANGE we extract a (lo, hi) interval
        // off the WHERE; for LIST / HASH we extract a single `=`
        // literal (and the rest of the planner falls back to "keep
        // every child" — same conservative path as 16.1/16.2).
        let (lo_bound, hi_bound) = match outer.where_.as_ref() {
            Some(expr) => extract_key_range(expr, &key_col_name),
            None => (None, None),
        };
        let eq_value: Option<spg_storage::Value<'static>> = match outer.where_.as_ref() {
            Some(expr) => extract_key_eq_value(expr, &key_col_name),
            None => None,
        };
        let children = crate::partition::children_of_parent(cat, parent_name);
        let mut kept: Vec<String> = Vec::new();
        let mut default_child: Option<String> = None;
        // First pass — apply per-strategy gates, defer DEFAULT until
        // we know whether some non-DEFAULT child matched.
        for child_name in &children {
            let Some(child) = cat.get(child_name) else {
                continue;
            };
            match &child.schema().partition_role {
                Some(PartitionRole::Range { lower, upper, .. }) => {
                    if range_satisfies_filter(lower, upper, lo_bound.as_ref(), hi_bound.as_ref()) {
                        kept.push(child_name.clone());
                    }
                }
                // v7.37.16 (16.7) — LIST pruning: if WHERE has `key
                // = <lit>`, only the child whose values contain that
                // literal survives. Otherwise (no equality predicate
                // or planner couldn't extract one) keep the child
                // conservatively.
                Some(PartitionRole::List { values, .. }) => match &eq_value {
                    Some(v) => {
                        if values.iter().any(|b| b.equals_value(v)) {
                            kept.push(child_name.clone());
                        }
                    }
                    None => kept.push(child_name.clone()),
                },
                // v7.37.16 (16.7) — HASH pruning: with `key = <lit>`
                // we know the residue class deterministically, so
                // only the matching REMAINDER child survives.
                Some(PartitionRole::Hash {
                    modulus, remainder, ..
                }) => match &eq_value {
                    Some(v) => {
                        let h = crate::partition::pg_compatible_hash(v);
                        if h.rem_euclid(u64::from(*modulus)) == u64::from(*remainder) {
                            kept.push(child_name.clone());
                        }
                    }
                    None => kept.push(child_name.clone()),
                },
                Some(PartitionRole::Default { .. }) => {
                    default_child = Some(child_name.clone());
                }
                _ => {}
            }
        }
        // PG-style DEFAULT semantics: the DEFAULT child must be
        // scanned iff some row could fall outside every concrete
        // child's bound predicate. We approximate that as "no
        // concrete child matched" (== full prune) — strictly
        // conservative for LIST / HASH (DEFAULT also catches rows
        // outside the union of value-sets / residues), and matches
        // PG for the equality case where we *do* know the routing
        // outcome.
        let _ = parent_kind; // used to silence dead-code lint while 16.8-9 lands.
        if let Some(d) = default_child {
            if kept.is_empty() {
                kept.push(d);
            } else if eq_value.is_none() {
                // Without an equality literal, the DEFAULT child may
                // still hold matching rows (e.g. LIKE on TEXT keys
                // for which a LIST partition exists). Keep it.
                kept.push(d);
            }
        }
        // Build the UNION ALL body text and re-parse — keeps the
        // rewrite expressible in surface SQL so the engine's existing
        // parser path handles the AST shape uniformly.
        if kept.is_empty() {
            // No children survive — caller falls back to scanning the
            // (empty) parent table. Returning None here is what
            // prevents the synthetic CTE from referring back to the
            // parent name and re-entering this rewrite pass.
            let _ = parent_name;
            return Ok(None);
        }
        // v7.39 (round 622, S05a) — the system columns of the CHILD the row
        // actually lives in.
        //
        // The parent is read through a synthetic CTE, so a `tableoid` on it
        // resolved against that CTE: every row of every child reported
        // `__spg_partition_pm`, an internal name no user ever typed, where
        // PG reports `pm_a` / `pm_b`. That is not only a leak — it silently
        // empties `WHERE tableoid::regclass::TEXT = 'pm_a'`, which is how
        // one asks "which partition is this row in", answering 0 rows where
        // PG answers 1. `ctid` had the same shape: it numbered the CTE's
        // output, so rows in different children got distinct ctids instead
        // of each child's own physical position.
        //
        // Naming them in the term is what carries them: the child scan
        // materialises its own six because the statement now references
        // them, and they land in SYSTEM_COLUMNS order right after the user
        // columns — the exact layout the positional `*` skip already
        // expects. Only done when the outer statement asks for one, so a
        // plain `SELECT * FROM parent` scans exactly what it scanned.
        let carry_sys = references_ctid(outer);
        let mut body = alloc::string::String::new();
        for (i, child_name) in kept.iter().enumerate() {
            if i > 0 {
                body.push_str(" UNION ALL ");
            }
            body.push_str("SELECT *");
            if carry_sys {
                for sys in SYSTEM_COLUMNS {
                    body.push_str(", ");
                    body.push_str(sys);
                }
            }
            body.push_str(" FROM ");
            body.push_str(&quote_ident_for_sql(child_name));
        }
        parse_select_or_corrupt(&body).map(Some)
    }
}

/// Rewrite a `TableRef` pointing at a partition parent so it
/// references the synthetic CTE created by the expansion. If the
/// original ref had no alias, preserve the parent name as an alias
/// so column references like `events_partitioned.received_at`
/// keep resolving.
fn rewrite_partition_parent_table_ref(
    t: &mut spg_sql::ast::TableRef,
    parents: &[alloc::string::String],
    synth_name: &impl Fn(&str) -> alloc::string::String,
) {
    if t.lateral_subquery.is_some() || t.unnest_expr.is_some() || t.generate_series_args.is_some() {
        return;
    }
    // v7.39 (round 644) — an ONLY reference stays pointed at the parent
    // itself. The rewrite is keyed on the NAME, so in
    // `FROM ONLY po a JOIN po b` the un-qualified `b` put `po` on the
    // parent list and this then rewrote BOTH — including the one that
    // asked not to descend. PG answers 0 for that join; SPG answered 2.
    // Folded into the existing test — see the note in
    // `collect_partition_parent_refs` for what a separate one cost.
    if t.only || !parents.iter().any(|p| p == &t.name) {
        return;
    }
    if t.alias.is_none() {
        t.alias = Some(t.name.clone());
    }
    t.name = synth_name(&t.name);
}

/// Walk a `TableRef` and push its `name` if it resolves to a partition
/// parent in `cat`. Skips `lateral_subquery` / `unnest_expr` /
/// `generate_series_args` references — those aren't catalog tables.
fn collect_partition_parent_refs(
    t: &spg_sql::ast::TableRef,
    cat: &spg_storage::Catalog,
    out: &mut Vec<alloc::string::String>,
) {
    if t.lateral_subquery.is_some() || t.unnest_expr.is_some() || t.generate_series_args.is_some() {
        return;
    }
    // v7.39 (round 644) — `FROM ONLY <parent>` scans the parent alone.
    // The keyword used to be absorbed at parse time, so this fanned out
    // anyway and `SELECT count(*) FROM ONLY <partitioned parent>`
    // answered 2 where PG answers 0.
    //
    // Folded into the existing test rather than given an early return of
    // its own: as two extra lines in this function's body it cost
    // `WHERE g BETWEEN 10 AND 20` **26x**, 5.9 ms to 155 ms, measured
    // outside the panel. Rounds 641 and 643 met the same wall from the
    // other two directions — adding to a hot function and taking away
    // from a cold one. What goes in a body near the row loop is a
    // codegen decision whatever its shape.
    if !t.only && crate::partition::has_children(cat, &t.name) {
        out.push(t.name.clone());
    }
}

/// v7.37.6-B partition-key range derived from a WHERE expression.
/// `i64` microseconds since epoch with the same sign convention as
/// `Value::Timestamp`. Inclusive bool: `true` ⇒ inclusive(`>=` / `<=`
/// / `=`),`false` ⇒ exclusive(`>` / `<`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PartitionFilterBound {
    pub micros: i64,
    pub inclusive: bool,
}

/// Walk a flat AND chain looking for `<key> <op> <timestamptz-literal>`
/// shapes; tighten the running lo / hi as we go. Anything outside that
/// (OR / nested calls / non-key columns)is ignored — caller treats
/// `None` as "no constraint on that side."
fn extract_key_range(
    expr: &spg_sql::ast::Expr,
    key_col: &str,
) -> (Option<PartitionFilterBound>, Option<PartitionFilterBound>) {
    let mut lo: Option<PartitionFilterBound> = None;
    let mut hi: Option<PartitionFilterBound> = None;
    let mut stack: Vec<&spg_sql::ast::Expr> = alloc::vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            spg_sql::ast::Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::And,
                rhs,
            } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            // BETWEEN is desugared at parse time into `lhs >= low AND
            // lhs <= high`, so it lands here as two regular Binary
            // arms via the AND walker above.
            spg_sql::ast::Expr::Binary { lhs, op, rhs } => {
                let (col_ref, lit_side, swapped) = if is_column_ref(lhs, key_col) {
                    (Some(lhs.as_ref()), rhs.as_ref(), false)
                } else if is_column_ref(rhs, key_col) {
                    (Some(rhs.as_ref()), lhs.as_ref(), true)
                } else {
                    (None, lhs.as_ref(), false)
                };
                if col_ref.is_none() {
                    continue;
                }
                let Some(lit) = literal_to_micros(lit_side) else {
                    continue;
                };
                use spg_sql::ast::BinOp::{Eq, Gt, GtEq, Lt, LtEq};
                let effective_op = if swapped {
                    match op {
                        Lt => Gt,
                        LtEq => GtEq,
                        Gt => Lt,
                        GtEq => LtEq,
                        other => *other,
                    }
                } else {
                    *op
                };
                match effective_op {
                    Eq => {
                        tighten_lo(
                            &mut lo,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: true,
                            },
                        );
                        tighten_hi(
                            &mut hi,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: true,
                            },
                        );
                    }
                    GtEq => {
                        tighten_lo(
                            &mut lo,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: true,
                            },
                        );
                    }
                    Gt => {
                        tighten_lo(
                            &mut lo,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: false,
                            },
                        );
                    }
                    LtEq => {
                        tighten_hi(
                            &mut hi,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: true,
                            },
                        );
                    }
                    Lt => {
                        tighten_hi(
                            &mut hi,
                            PartitionFilterBound {
                                micros: lit,
                                inclusive: false,
                            },
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    (lo, hi)
}

fn tighten_lo(slot: &mut Option<PartitionFilterBound>, new: PartitionFilterBound) {
    match slot {
        None => *slot = Some(new),
        Some(cur) => {
            if new.micros > cur.micros
                || (new.micros == cur.micros && !new.inclusive && cur.inclusive)
            {
                *slot = Some(new);
            }
        }
    }
}

fn tighten_hi(slot: &mut Option<PartitionFilterBound>, new: PartitionFilterBound) {
    match slot {
        None => *slot = Some(new),
        Some(cur) => {
            if new.micros < cur.micros
                || (new.micros == cur.micros && !new.inclusive && cur.inclusive)
            {
                *slot = Some(new);
            }
        }
    }
}

fn is_column_ref(e: &spg_sql::ast::Expr, key_col: &str) -> bool {
    if let spg_sql::ast::Expr::Column(c) = e {
        c.name.eq_ignore_ascii_case(key_col)
    } else {
        false
    }
}

/// v7.37.16 (16.7) — walk an AND-chain WHERE and pull a single
/// `key_col = <literal>` predicate out for LIST/HASH partition
/// pruning. Returns `None` when no equality literal can be lifted
/// (planner then keeps every child — correctness preserved). The
/// returned `Value<'static>` is an owned coercion so the caller can
/// outlive any AST node it was extracted from.
pub(crate) fn extract_key_eq_value(
    expr: &spg_sql::ast::Expr,
    key_col: &str,
) -> Option<spg_storage::Value<'static>> {
    let mut stack: Vec<&spg_sql::ast::Expr> = alloc::vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            spg_sql::ast::Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::And,
                rhs,
            } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            spg_sql::ast::Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } => {
                let lit_side = if is_column_ref(lhs, key_col) {
                    rhs.as_ref()
                } else if is_column_ref(rhs, key_col) {
                    lhs.as_ref()
                } else {
                    continue;
                };
                let cloned = lit_side.clone();
                let Ok(v) = crate::conversions::literal_expr_to_value(cloned) else {
                    continue;
                };
                // Coerce to an owned Value<'static> so the caller
                // can hold it past the WHERE expression's lifetime.
                let owned: spg_storage::Value<'static> = match v {
                    spg_storage::Value::Text(s) => {
                        spg_storage::Value::Text(alloc::borrow::Cow::Owned(s.into_owned()))
                    }
                    spg_storage::Value::SmallInt(n) => spg_storage::Value::SmallInt(n),
                    spg_storage::Value::Int(n) => spg_storage::Value::Int(n),
                    spg_storage::Value::BigInt(n) => spg_storage::Value::BigInt(n),
                    spg_storage::Value::Date(d) => spg_storage::Value::Date(d),
                    spg_storage::Value::Timestamp(t) => spg_storage::Value::Timestamp(t),
                    spg_storage::Value::Bool(b) => spg_storage::Value::Bool(b),
                    spg_storage::Value::Null => spg_storage::Value::Null,
                    // Anything else (Vector / Json / Bytes / Numeric /
                    // arrays / interval / …) isn't a current partition
                    // key type; skip without pruning.
                    _ => continue,
                };
                return Some(owned);
            }
            _ => {}
        }
    }
    None
}

/// Coerce a literal Expr(after the parser folded sequence calls etc.)
/// to i64 microseconds. Mirrors `evaluate_partition_bound`'s shape so
/// pruning and routing agree on the literal vocabulary. Returns
/// `None` when the literal isn't recognised(planner then skips
/// pruning on that branch — correctness preserved).
fn literal_to_micros(e: &spg_sql::ast::Expr) -> Option<i64> {
    let cloned = e.clone();
    let value = crate::conversions::literal_expr_to_value(cloned).ok()?;
    match value {
        spg_storage::Value::Timestamp(m) => Some(m),
        spg_storage::Value::Date(days) => Some(i64::from(days) * 86_400i64 * 1_000_000i64),
        spg_storage::Value::Text(s) => crate::eval::parse_timestamp_literal(&s),
        _ => None,
    }
}

/// `[range_lo, range_hi)` of a child is kept iff it can hold any row
/// satisfying the WHERE-derived filter range. PG-style half-open:
/// child upper exclusive. Filter inclusivity is honoured per-bound.
fn range_satisfies_filter(
    range_lo: &spg_storage::PartitionBound,
    range_hi: &spg_storage::PartitionBound,
    filter_lo: Option<&PartitionFilterBound>,
    filter_hi: Option<&PartitionFilterBound>,
) -> bool {
    use spg_storage::PartitionBound;
    // For each filter side, reject children that can't host any row
    // matching the predicate.
    if let Some(lo) = filter_lo {
        // child upper bound vs filter lower:
        //   if filter is x >= L, child rejects iff child.hi <= L
        //   if filter is x  > L, child rejects iff child.hi <= L
        //   (child.hi exclusive, so equality with L still rejects)
        match range_hi {
            PartitionBound::MinValue => return false,
            PartitionBound::MaxValue => {}
            PartitionBound::TimestampTz(hi) => {
                if *hi <= lo.micros {
                    return false;
                }
            }
            // v7.37.16 (16.6) — non-TIMESTAMPTZ bounds aren't
            // matched against TIMESTAMPTZ filters here; keep child
            // (conservative: don't prune).
            PartitionBound::BigInt(_)
            | PartitionBound::Int(_)
            | PartitionBound::SmallInt(_)
            | PartitionBound::Date(_)
            | PartitionBound::Text(_) => {}
        }
    }
    if let Some(hi) = filter_hi {
        // child lower bound vs filter upper:
        //   if filter is x <= U, child rejects iff child.lo > U
        //   if filter is x  < U, child rejects iff child.lo >= U
        match range_lo {
            PartitionBound::MaxValue => return false,
            PartitionBound::MinValue => {}
            PartitionBound::TimestampTz(lo) => {
                let rejects = if hi.inclusive {
                    *lo > hi.micros
                } else {
                    *lo >= hi.micros
                };
                if rejects {
                    return false;
                }
            }
            PartitionBound::BigInt(_)
            | PartitionBound::Int(_)
            | PartitionBound::SmallInt(_)
            | PartitionBound::Date(_)
            | PartitionBound::Text(_) => {}
        }
    }
    true
}

fn quote_ident_for_sql(name: &str) -> alloc::string::String {
    // Match spg-sql's quoting rule(unquoted when ASCII-lowercase
    // identifier, otherwise quoted). Conservative: always quote so
    // children with reserved names round-trip safely through the
    // CTE-body parse.
    let mut out = alloc::string::String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn parse_select_or_corrupt(sql: &str) -> Result<SelectStatement, EngineError> {
    let parsed = spg_sql::parser::parse_statement(sql).map_err(|e| {
        EngineError::Unsupported(alloc::format!(
            "partition expansion: generated SQL {sql:?} failed to re-parse: {e}"
        ))
    })?;
    let Statement::Select(body) = parsed else {
        return Err(EngineError::Unsupported(alloc::format!(
            "partition expansion: generated SQL {sql:?} is not a SELECT"
        )));
    };
    Ok(body)
}

/// v7.39 (read01 round 65/66) — the column shape a set-returning function
/// exposes. `RETURNS TABLE(id int, v text)` names them; a `SETOF <scalar>`
/// yields ONE column named after the call's alias when there is one (`FROM
/// odds() AS x` → `x`), else after the function. Get this wrong and the alias
/// resolves to the whole ROW: `SELECT x::text FROM odds() AS x` renders `(1)`.
fn setof_column_shape_from(
    declared: &str,
    name: &str,
    alias: Option<&str>,
    got: &[ColumnSchema],
) -> alloc::vec::Vec<ColumnSchema> {
    let upper = declared.to_ascii_uppercase();
    if upper.starts_with("TABLE(") {
        let raw = &declared["TABLE(".len()..declared.len() - 1];
        return raw
            .split(',')
            .zip(got.iter())
            .map(|(decl, g)| {
                let cname = decl.split_whitespace().next().unwrap_or(g.name.as_str());
                ColumnSchema::new(cname.to_string(), g.ty, true)
            })
            .collect();
    }
    let cname = alias.unwrap_or(name);
    got.first()
        .map(|c| alloc::vec![ColumnSchema::new(cname.to_string(), c.ty, true)])
        .unwrap_or_default()
}

/// The plpgsql twin: the interpreter hands back raw value rows, so the types
/// come off the first row.
fn setof_column_shape(
    declared: &str,
    name: &str,
    alias: Option<&str>,
    first_row: Option<&alloc::vec::Vec<Value<'static>>>,
) -> alloc::vec::Vec<ColumnSchema> {
    let got: alloc::vec::Vec<ColumnSchema> = first_row
        .map(|r| {
            r.iter()
                .enumerate()
                .map(|(i, v)| {
                    ColumnSchema::new(
                        alloc::format!("col{i}"),
                        v.data_type().unwrap_or(DataType::Text),
                        true,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    setof_column_shape_from(declared, name, alias, &got)
}

/// v7.39 (read01 round 67) — expand every set-returning call in a target list
/// for ONE input row, PG's ProjectSet semantics.
///
/// Several SRFs in one list run in **LOCKSTEP**, not as a cross product: the
/// output has as many rows as the LONGEST of them, and a shorter one is padded
/// with NULLs. (`SELECT generate_series(1,3), generate_series(10,11)` →
/// `1/10, 2/11, 3/NULL`.) A single SRF is the degenerate case of that, and an
/// SRF that yields no rows at all contributes none — `SELECT unnest('{}'::int[])`
/// is zero rows, not one NULL row.
///
/// Non-SRF items repeat, evaluated once per output row from the same input row.
/// v7.39 (read01 round 79) — where an aggregate may NOT appear. Both of these
/// used to reach the scalar function dispatcher, which reported the aggregate as
/// an *unknown function* — the same "symptom two layers above the cause" shape
/// round 78 found with SRFs. Neither can be diagnosed down there: the dispatcher
/// sees a call, not the clause it came from. The statement knows.
/// v7.39 (round 294, E3 Phase 1b) — PG's rules on WHERE a row-locking
/// clause may appear.
///
/// PG rejects `FOR UPDATE` on exactly the shapes that have no
/// identifiable base row to lock, each with its own wording. SPG
/// accepted all of them and locked nothing, so a query that PG refuses
/// outright came back looking like it had taken locks.
///
/// Every wording read off live PG 18.4.
fn validate_locking_clause(stmt: &SelectStatement) -> Result<(), EngineError> {
    let Some(lock) = &stmt.locking else {
        return Ok(());
    };
    let verb = lock_clause_verb(lock.strength);
    let refuse = |what: &str| {
        Err(EngineError::Unsupported(alloc::format!(
            "{verb} is not allowed with {what}"
        )))
    };
    if !stmt.unions.is_empty() {
        return refuse("UNION/INTERSECT/EXCEPT");
    }
    if stmt.distinct || !stmt.distinct_on.is_empty() {
        return refuse("DISTINCT clause");
    }
    if stmt.group_by.is_some() || stmt.group_by_all {
        return refuse("GROUP BY clause");
    }
    let has_agg = stmt.items.iter().any(|it| match it {
        spg_sql::ast::SelectItem::Expr { expr, .. } => crate::aggregate::contains_aggregate(expr),
        _ => false,
    });
    if has_agg {
        return refuse("aggregate functions");
    }
    // `FOR UPDATE OF t` must name a relation that is actually in FROM.
    for want in &lock.of_tables {
        if !locking_from_names(stmt)
            .iter()
            .any(|n| n.eq_ignore_ascii_case(want))
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "relation \"{want}\" in {verb} clause not found in FROM clause"
            )));
        }
    }
    Ok(())
}

/// How PG names the clause in its diagnostics.
const fn lock_clause_verb(s: spg_sql::ast::LockStrength) -> &'static str {
    use spg_sql::ast::LockStrength as LS;
    match s {
        LS::Update => "FOR UPDATE",
        LS::NoKeyUpdate => "FOR NO KEY UPDATE",
        LS::Share => "FOR SHARE",
        LS::KeyShare => "FOR KEY SHARE",
    }
}

/// Every relation name (or alias) the FROM clause exposes.
fn locking_from_names(stmt: &SelectStatement) -> alloc::vec::Vec<String> {
    let mut out = alloc::vec::Vec::new();
    if let Some(f) = &stmt.from {
        let mut push = |t: &spg_sql::ast::TableRef| {
            if let Some(a) = &t.alias {
                out.push(a.clone());
            }
            out.push(t.name.clone());
        };
        push(&f.primary);
        for j in &f.joins {
            push(&j.table);
        }
    }
    out
}

fn validate_aggregate_placement(stmt: &SelectStatement) -> Result<(), EngineError> {
    use spg_sql::ast::Expr;
    if let Some(w) = &stmt.where_
        && aggregate::contains_aggregate(w)
    {
        return Err(EngineError::Unsupported(
            "aggregate functions are not allowed in WHERE".into(),
        ));
    }
    let mut nested = false;
    let mut check = |e: &Expr| {
        let mut probe = e.clone();
        crate::expr_analysis::rewrite_nodes_mut(&mut probe, &mut |n| {
            let args = match n {
                Expr::FunctionCall { name, args } if aggregate::is_aggregate_name(name) => args,
                _ => return false,
            };
            if args.iter().any(aggregate::contains_aggregate) {
                nested = true;
            }
            false
        });
    };
    for it in &stmt.items {
        if let spg_sql::ast::SelectItem::Expr { expr, .. } = it {
            check(expr);
        }
    }
    if let Some(h) = &stmt.having {
        check(h);
    }
    for o in &stmt.order_by {
        check(&o.expr);
    }
    if nested {
        return Err(EngineError::Unsupported(
            "aggregate function calls cannot be nested".into(),
        ));
    }
    Ok(())
}

/// v7.39 (read01 round 78) — an SRF may sit ANYWHERE inside a target-list
/// expression, not only as the whole item: `upper(unnest(a))`, `unnest(a) + 10`,
/// `'x:' || unnest(a)`, `(regexp_matches(s, p, 'g'))::text`. PG evaluates the SRF
/// to a set and then applies the enclosing expression once per element. SPG only
/// ever recognised an SRF that WAS the item, so everything above died on
/// "unknown function unnest" — the set-returning call, wrapped in anything at
/// all, fell through to the scalar function dispatcher which has no such name.
///
/// Each SRF node is lifted out into a synthetic column (`__srf_k`), the tree is
/// rewritten to read that column, and the rewritten expression is evaluated once
/// per output row against the input row extended with the lifted values. The
/// lift is by VALUE, not by literal: a text[] or a jsonb keeps its type exactly.
/// v7.39 (read01 round 80) — `ORDER BY <n>` names the Nth OUTPUT column. Three
/// executors (the single-table scan, the synthetic-table pipeline, and the
/// unnest FROM path) each evaluated the key as an ordinary expression, where the
/// literal `n` is just the constant n — the same sort key for every row. The
/// sort therefore ran and changed nothing, which is why nobody noticed: rows came
/// back in input order, not in a wrong order. Statement prep resolves the common
/// case, but only when the SELECT item is an expression — a `*` is not one, and
/// `SELECT unnest(a) x` becomes `SELECT * FROM unnest(a) x`, so the everyday
/// spelling landed on exactly the shape prep could not resolve.
///
/// A set-returning item is left alone: copying it into ORDER BY would make the
/// key "the whole set", evaluated once per INPUT row.
fn resolve_positional_order_by(
    order_by: &[spg_sql::ast::OrderBy],
    projection: &[ProjectedItem],
) -> alloc::vec::Vec<spg_sql::ast::OrderBy> {
    order_by
        .iter()
        .map(|o| {
            let mut o = o.clone();
            if let Expr::Literal(spg_sql::ast::Literal::Integer(n)) = &o.expr
                && *n >= 1
                && let Ok(idx) = usize::try_from(*n - 1)
                && let Some(item) = projection.get(idx)
                && !expr_contains_builtin_srf(&item.expr)
            {
                o.expr = item.expr.clone();
            }
            o
        })
        .collect()
}

/// v7.39 (read01 round 80) — does a BUILTIN set-returning call appear anywhere in
/// this expression? Statement preparation (`resolve_order_by_position`) runs
/// before any catalog is in hand, and it only needs to know "is this item's value
/// a set", which the builtin SRFs answer syntactically.
pub(crate) fn expr_contains_builtin_srf(e: &spg_sql::ast::Expr) -> bool {
    let mut found = false;
    let mut probe = e.clone();
    crate::expr_analysis::rewrite_nodes_mut(&mut probe, &mut |n| {
        if is_top_level_unnest(n) {
            found = true;
            return true;
        }
        false
    });
    found
}

/// v7.39 (round 599) — everything about a target-list SRF that does not
/// depend on the row.
///
/// `expand_srf_row` derived all of this again for EVERY input row: it cloned
/// each SRF-bearing projection expression, walked and rewrote the tree,
/// formatted a `__srf_N` name per node, and copied the whole column schema.
/// A counting allocator put the path at 24 allocations per input row for a
/// single-element `unnest`, against 0 for the same scan without one — 211 MB
/// where the plain scan took 4.3 — and the shape held whatever the array
/// contained, which is what invariant work looks like.
struct SrfPlan {
    /// The lifted SRF calls, in slot order.
    nodes: alloc::vec::Vec<spg_sql::ast::Expr>,
    /// Per projection position, the expression with its SRF calls replaced
    /// by `__srf_N` column references. `None` means the item has none.
    rewritten: alloc::vec::Vec<Option<spg_sql::ast::Expr>>,
    /// The input schema followed by one column per slot. Only the slots'
    /// TYPES vary per row, and they are patched in place.
    ext_cols: alloc::vec::Vec<ColumnSchema>,
    /// v7.39 (round 743) — the rewritten projection COMPILED against the
    /// extended schema, once per plan. The per-output-row evaluation ran
    /// the interpreter (~560 ns/row on the unnest panel cell); the Step
    /// VM reads the `__srf_N` slots as plain columns. `None` = that item
    /// is not fully compilable and keeps the interpreter.
    compiled: alloc::vec::Vec<Option<eval::CompiledExpr>>,
    base_cols: usize,
}

fn build_srf_plan(
    engine: &Engine,
    projection: &[ProjectedItem],
    srf_idxs: &[usize],
    ctx: &EvalContext<'_>,
) -> Result<SrfPlan, EngineError> {
    // Lift every SRF node out of every item that contains one.
    let mut nodes: Vec<spg_sql::ast::Expr> = Vec::new();
    let mut rewritten: Vec<Option<spg_sql::ast::Expr>> = alloc::vec![None; projection.len()];
    let mut reject: Option<EngineError> = None;
    for &i in srf_idxs {
        let mut e = projection[i].expr.clone();
        crate::expr_analysis::rewrite_nodes_mut(&mut e, &mut |n| {
            if reject.is_some() {
                return true;
            }
            // PG refuses a set-returning function inside a conditional: the set
            // would have to be produced before anyone knows whether the branch
            // is even taken.
            let conditional = match n {
                spg_sql::ast::Expr::Case { .. } => Some("CASE"),
                spg_sql::ast::Expr::FunctionCall { name, .. }
                    if name.eq_ignore_ascii_case("coalesce") =>
                {
                    Some("COALESCE")
                }
                _ => None,
            };
            if let Some(kind) = conditional
                && engine.expr_contains_srf(n)
            {
                reject = Some(EngineError::Unsupported(alloc::format!(
                    "set-returning functions are not allowed in {kind}"
                )));
                return true;
            }
            if !engine.is_srf_node(n) {
                return false;
            }
            let slot = nodes.len();
            nodes.push(n.clone());
            *n = spg_sql::ast::Expr::Column(spg_sql::ast::ColumnName {
                qualifier: None,
                name: alloc::format!("__srf_{slot}"),
            });
            true
        });
        rewritten[i] = Some(e);
    }
    if let Some(err) = reject {
        return Err(err);
    }
    let base_cols = ctx.columns.len();
    let mut ext_cols: Vec<ColumnSchema> = ctx.columns.to_vec();
    for slot in 0..nodes.len() {
        ext_cols.push(ColumnSchema::new(
            alloc::format!("__srf_{slot}"),
            DataType::Text,
            true,
        ));
    }
    // v7.39 (round 743) — compile the rewritten items against the
    // EXTENDED schema. The slot columns' declared type is a per-row
    // patched detail the compiled column read does not consult.
    let compiled: Vec<Option<eval::CompiledExpr>> = {
        let mut ext_ctx = ctx.clone();
        ext_ctx.columns = &ext_cols;
        projection
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let e = rewritten[i].as_ref().unwrap_or(&p.expr);
                if eval::fully_compilable(e) {
                    Some(eval::compile_expr(e, &ext_ctx))
                } else {
                    None
                }
            })
            .collect()
    };
    Ok(SrfPlan {
        nodes,
        rewritten,
        ext_cols,
        compiled,
        base_cols,
    })
}

/// One input row expanded through a plan built once for the whole scan.
/// v7.39 (round 621) — expand a projection whose target list contains
/// set-returning items, remembering which INPUT row each output row came from.
///
/// The three materialised-source tails — `FROM unnest(…)`, `FROM
/// generate_series(…)`, and the one that serves VALUES / a derived table /
/// `ROWS FROM (…)` — are near-copies of each other, and only the first knew
/// about target-list SRFs. So `SELECT unnest(ARRAY[1,2]), x FROM (VALUES (3),(4))
/// v(x)` answered `function unnest(integer[]) does not exist` on all the
/// others, for a query PG answers. Sharing the expansion is the point: a
/// fourth copy would have been the fourth place to forget.
fn expand_projection_srfs(
    engine: &Engine,
    projection: &[ProjectedItem],
    srf_idxs: &[usize],
    filtered: &[Row<'static>],
    ctx: &EvalContext<'_>,
) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<usize>), EngineError> {
    let mut out = alloc::vec::Vec::with_capacity(filtered.len());
    let mut src = alloc::vec::Vec::with_capacity(filtered.len());
    // v7.39 (round 726) — ONE plan for the whole scan. The per-row
    // spelling rebuilt it for every input row: a full clone of the
    // rewritten projection trees and the extended schema, 50k times on
    // the panel's unnest cell.
    let mut plan = build_srf_plan(engine, projection, srf_idxs, ctx)?;
    // v7.39 (round 733) — shard the expansion. Each shard clones the
    // plan (its ext_cols slot types are per-row mutable) and builds a
    // MINIMAL context — EvalContext is not Sync — which is sound only
    // when every expression involved is pure: the whole projection and
    // every SRF argument must be fully_compilable, or the row loop
    // stays serial with the full session context.
    // The projection is judged in its REWRITTEN form — the SRF call
    // itself is never compilable, but after the lift it is a plain
    // `__srf_N` column reference.
    let all_pure = projection
        .iter()
        .enumerate()
        .all(|(i, p)| eval::fully_compilable(plan.rewritten[i].as_ref().unwrap_or(&p.expr)))
        && plan.nodes.iter().all(|n| match n {
            Expr::FunctionCall { args, .. } => args.iter().all(eval::fully_compilable),
            other => eval::fully_compilable(other),
        });
    if all_pure
        && filtered.len() >= crate::PARALLEL_MIN_ROWS / 5
        && let Some(r) = engine.parallel_runner.0.as_deref()
    {
        let n_shards = (filtered.len() / (crate::PARALLEL_MIN_ROWS / 5)).clamp(2, 8);
        let chunk = filtered.len().div_ceil(n_shards);
        type ShardOut = Result<(Vec<Row<'static>>, Vec<usize>), EngineError>;
        let schema_cols = ctx.columns;
        let alias = ctx.table_alias;
        let mysql = ctx.mysql_dialect;
        let style = ctx.render_style;
        let plan_ref = &plan;
        let results = r.run_shards(n_shards, &|si| {
            let lo = si * chunk;
            let hi = ((si + 1) * chunk).min(filtered.len());
            let mut sctx = eval::EvalContext::new(schema_cols, alias);
            sctx.mysql_dialect = mysql;
            sctx.render_style = style;
            // v7.39 (round 743) — SrfPlan is no longer Clone (it carries
            // compiled programs); each shard rebuilds it, which also
            // recompiles against the shard's own context. Build errors
            // were already surfaced by the outer build above.
            let mut local_plan = match build_srf_plan(engine, projection, srf_idxs, &sctx) {
                Ok(p) => p,
                Err(e) => return alloc::boxed::Box::new(ShardOut::Err(e)) as _,
            };
            let mut run = || -> ShardOut {
                let mut o: Vec<Row<'static>> = Vec::with_capacity(hi - lo);
                let mut sidx: Vec<usize> = Vec::with_capacity(hi - lo);
                for (i, row) in filtered[lo..hi].iter().enumerate() {
                    let expanded =
                        expand_srf_row_with(engine, &mut local_plan, projection, row, &sctx)?;
                    sidx.extend(core::iter::repeat_n(lo + i, expanded.len()));
                    o.extend(expanded);
                }
                Ok((o, sidx))
            };
            alloc::boxed::Box::new(run())
        });
        for boxed in results {
            let shard = boxed
                .downcast::<ShardOut>()
                .expect("runner echoes the closure's box");
            let (o, sidx) = (*shard)?;
            out.extend(o);
            src.extend(sidx);
        }
        return Ok((out, src));
    }
    for (i, row) in filtered.iter().enumerate() {
        let expanded = expand_srf_row_with(engine, &mut plan, projection, row, ctx)?;
        src.extend(core::iter::repeat_n(i, expanded.len()));
        out.extend(expanded);
    }
    Ok((out, src))
}

/// v7.39 (round 621) — one ORDER BY key, read from wherever it lives.
///
/// A key that names a select-list item reads it out of the EXPANDED row,
/// because PG sorts after the expansion. A key that names a source column the
/// query does not project is evaluated against the input row that output row
/// came from. `out_col` is `srf_order_output_cols`'s verdict for this key.
fn srf_order_key(
    ob: &spg_sql::ast::OrderBy,
    out_col: Option<usize>,
    out: &Row<'static>,
    src: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EngineError> {
    match out_col {
        Some(i) => Ok(out.values.get(i).cloned().unwrap_or(Value::Null)),
        None => eval::eval_expr(&ob.expr, src, ctx).map_err(EngineError::Eval),
    }
}

fn expand_srf_row_with(
    engine: &Engine,
    plan: &mut SrfPlan,
    projection: &[ProjectedItem],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Row<'static>>, EngineError> {
    let mut lists: Vec<Vec<Value<'static>>> = Vec::with_capacity(plan.nodes.len());
    for n in &plan.nodes {
        lists.push(engine.srf_values(n, row, ctx)?);
    }
    let n_rows = lists.iter().map(Vec::len).max().unwrap_or(0);
    // Only the slots' element types depend on the row; the names and the
    // input schema around them do not.
    for (slot, list) in lists.iter().enumerate() {
        plan.ext_cols[plan.base_cols + slot].ty = list
            .iter()
            .find_map(|v| v.data_type())
            .unwrap_or(DataType::Text);
    }
    let mut ext_ctx = ctx.clone();
    ext_ctx.columns = &plan.ext_cols;
    let mut out = Vec::with_capacity(n_rows);
    // v7.39 (round 726) — the base columns are the SAME for every
    // expanded row; clone them once and rewrite only the SRF slots per
    // k. The old form cloned the whole input row per OUTPUT row — for
    // `unnest(ARRAY[id, g])` over d that was a 100k-fold clone of a
    // TEXT column the projection never reads.
    let base_len = row.values.len();
    let mut ext_vals = row.values.clone();
    ext_vals.resize(base_len + lists.len(), Value::Null);
    let mut eval_stack: alloc::vec::Vec<Value<'static>> = alloc::vec::Vec::new();
    for k in 0..n_rows {
        for (slot, list) in lists.iter().enumerate() {
            // Past the end of THIS srf's rows → NULL (PG pads).
            ext_vals[base_len + slot] = list.get(k).cloned().unwrap_or(Value::Null);
        }
        let ext_row = Row::new(core::mem::take(&mut ext_vals));
        let mut vals = Vec::with_capacity(projection.len());
        for (i, p) in projection.iter().enumerate() {
            // v7.39 (round 743) — compiled when possible; the
            // interpreter for the rest, with its exact wording.
            vals.push(match &plan.compiled[i] {
                Some(c) => eval::eval_compiled(c, &ext_row, &ext_ctx, &mut eval_stack)
                    .map_err(EngineError::Eval)?,
                None => {
                    let expr = plan.rewritten[i].as_ref().unwrap_or(&p.expr);
                    eval::eval_expr(expr, &ext_row, &ext_ctx).map_err(EngineError::Eval)?
                }
            });
        }
        ext_vals = ext_row.values;
        out.push(Row::new(vals));
    }
    Ok(out)
}

/// The one-shot spelling, for the callers that expand a single row.
/// v7.39 (round 600) — which output column each ORDER BY key names, for a
/// query whose target list contains a set-returning function.
///
/// The keys used to be built from the INPUT row, before the SRF expanded, so
/// anything that named the SRF's own output was evaluated as a scalar call:
/// `SELECT unnest(ARRAY[g,id]) v FROM sr ORDER BY v` answered
/// "function unnest(integer[]) does not exist", and so did the spellings that
/// repeat the call or reach it through `ORDER BY 1`. Where it did not error
/// it silently did nothing — `SELECT DISTINCT unnest(…) … ORDER BY 1` came
/// back in input order. PG sorts AFTER the expansion, so a key that names a
/// select-list item reads that item's value out of the expanded row.
///
/// `None` keeps the key on the input row, which is where an ORDER BY naming
/// a column the query does not project has to be evaluated.
fn srf_order_output_cols(
    order_by: &[spg_sql::ast::OrderBy],
    projection: &[ProjectedItem],
) -> Vec<Option<usize>> {
    order_by
        .iter()
        .map(|ob| {
            // A positive ordinal is the Nth output column, directly.
            // `resolve_positional_order_by` deliberately leaves an ordinal
            // pointing at a set-returning item alone — copying the call into
            // ORDER BY would have made the key "the whole set" back when keys
            // came from the input row. Reading the expanded row's column is
            // what it should have meant, and is what this does.
            if let Expr::Literal(spg_sql::ast::Literal::Integer(n)) = &ob.expr
                && *n >= 1
                && let Ok(idx) = usize::try_from(*n - 1)
                && idx < projection.len()
            {
                return Some(idx);
            }
            // An unqualified name matching exactly one output name. SQL
            // resolves ORDER BY against the select list first, so this wins
            // over an input column of the same name — which is the whole
            // point of `SELECT g AS id … ORDER BY id`.
            if let Expr::Column(c) = &ob.expr
                && c.qualifier.is_none()
            {
                let mut hit = None;
                for (i, p) in projection.iter().enumerate() {
                    if p.output_name.eq_ignore_ascii_case(&c.name) {
                        if hit.is_some() {
                            hit = None;
                            break;
                        }
                        hit = Some(i);
                    }
                }
                if hit.is_some() {
                    return hit;
                }
            }
            // Or the same expression as a select-list item — which is what
            // `ORDER BY 1` becomes once `resolve_positional_order_by` has
            // run, and what a repeated `ORDER BY unnest(…)` is.
            projection.iter().position(|p| p.expr == ob.expr)
        })
        .collect()
}

fn expand_srf_row(
    engine: &Engine,
    projection: &[ProjectedItem],
    srf_idxs: &[usize],
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Row<'static>>, EngineError> {
    let mut plan = build_srf_plan(engine, projection, srf_idxs, ctx)?;
    expand_srf_row_with(engine, &mut plan, projection, row, ctx)
}

impl Engine {
    /// The rows one target-list SRF yields for an input row. `None` from
    /// `srf_target_idxs` means the expression is not set-returning at all.
    fn srf_values(
        &self,
        expr: &spg_sql::ast::Expr,
        row: &Row<'static>,
        ctx: &EvalContext<'_>,
    ) -> Result<Vec<Value<'static>>, EngineError> {
        if top_level_srf_kind(expr).is_some() {
            return top_level_srf_output(expr, row, ctx);
        }
        // A user set-returning function. Its body runs through the real
        // executor, like every function body since round 63.
        let spg_sql::ast::Expr::FunctionCall { name, args } = expr else {
            return Err(EngineError::Unsupported(
                "expected a SELECT-list SRF call".into(),
            ));
        };
        let mut vals: alloc::vec::Vec<Value<'static>> = alloc::vec::Vec::new();
        for a in args {
            vals.push(eval::eval_expr(a, row, ctx).map_err(EngineError::Eval)?);
        }
        let (rows, cols) = self.setof_rows_of(name, &vals, None)?;
        // v7.39 (read01 round 68) — in a target list a multi-column function is
        // a RECORD, one composite value per row: `SELECT rows_of(2)` gives
        // `(2,b)`, `(3,c)`. Value::Composite has existed since round 56; this is
        // what it is for. A single-column function contributes its bare value.
        Ok(rows
            .into_iter()
            .map(|r| {
                if r.values.len() == 1 {
                    r.values.into_iter().next().unwrap_or(Value::Null)
                } else {
                    Value::Composite(
                        cols.iter()
                            .map(|c| c.name.clone())
                            .zip(r.values)
                            .collect::<alloc::vec::Vec<_>>(),
                    )
                }
            })
            .collect())
    }

    /// Is THIS node a set-returning call: one of the builtin kinds, or a user
    /// function declared `RETURNS SETOF` / `RETURNS TABLE`.
    fn is_srf_node(&self, e: &spg_sql::ast::Expr) -> bool {
        if is_top_level_unnest(e) {
            return true;
        }
        let spg_sql::ast::Expr::FunctionCall { name, .. } = e else {
            return false;
        };
        self.active_catalog().functions_named(name).iter().any(|f| {
            let r = f.returns.trim().to_ascii_uppercase();
            r.starts_with("SETOF") || r.starts_with("TABLE(")
        })
    }

    /// Does an SRF appear ANYWHERE in this expression (not only as its root)?
    fn expr_contains_srf(&self, e: &spg_sql::ast::Expr) -> bool {
        let mut found = false;
        let mut probe = e.clone();
        crate::expr_analysis::rewrite_nodes_mut(&mut probe, &mut |n| {
            if self.is_srf_node(n) {
                found = true;
                return true;
            }
            false
        });
        found
    }

    /// Which projection items CONTAIN a set-returning call. Before round 78 this
    /// asked whether the item WAS one, so `upper(unnest(a))` looked like an
    /// ordinary scalar call all the way down to the function dispatcher, which
    /// then reported `unnest` as an unknown function.
    fn srf_target_idxs(&self, projection: &[ProjectedItem]) -> alloc::vec::Vec<usize> {
        projection
            .iter()
            .enumerate()
            .filter(|(_, p)| self.expr_contains_srf(&p.expr))
            .map(|(i, _)| i)
            .collect()
    }
}

impl Engine {
    /// v7.39 (read01 round 74) — see the call site. `None` when the statement has
    /// no `(f(args)).*` item.
    fn lower_record_expansion(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        use spg_sql::ast::{Expr, SelectItem};
        let is_marker = |it: &SelectItem| {
            matches!(it, SelectItem::Expr { expr: Expr::FunctionCall { name, .. }, .. }
                if name == "__record_expand")
        };
        if !stmt.items.iter().any(is_marker) {
            return Ok(None);
        }
        let mut out = stmt.clone();
        let mut items: alloc::vec::Vec<SelectItem> = alloc::vec::Vec::new();
        let mut lateral_refs: alloc::vec::Vec<TableRef> = alloc::vec::Vec::new();
        for (n, item) in stmt.items.iter().enumerate() {
            if !is_marker(item) {
                items.push(item.clone());
                continue;
            }
            let SelectItem::Expr {
                expr: Expr::FunctionCall { args, .. },
                ..
            } = item
            else {
                unreachable!("checked by is_marker");
            };
            let Some(Expr::FunctionCall {
                name: fname,
                args: fargs,
            }) = args.first()
            else {
                return Err(EngineError::Unsupported(
                    "(<expr>).* expands a function's record — it needs a function call".into(),
                ));
            };
            let cols = self.setof_declared_columns(fname)?;
            let alias = alloc::format!("__rec{n}");
            let mut tref = bare_table_ref_named(&alias);
            tref.table_fn_call = Some(alloc::boxed::Box::new((
                fname.to_ascii_lowercase(),
                fargs.clone(),
            )));
            tref.alias = Some(alias.clone());
            lateral_refs.push(tref);
            for c in cols {
                items.push(SelectItem::Expr {
                    expr: Expr::Column(spg_sql::ast::ColumnName {
                        qualifier: Some(alias.clone()),
                        name: c,
                    }),
                    alias: None,
                });
            }
        }
        out.items = items;
        // The function joins the FROM. With no FROM it BECOMES the FROM; with one
        // it is a cross join, which is what `SELECT …, (f(t.c)).* FROM t` means
        // (the arguments may reference the outer row — the round-69 correlation).
        for tref in lateral_refs {
            match &mut out.from {
                None => {
                    out.from = Some(spg_sql::ast::FromClause {
                        primary: tref,
                        joins: alloc::vec::Vec::new(),
                    });
                }
                Some(from) => from.joins.push(spg_sql::ast::FromJoin {
                    kind: spg_sql::ast::JoinKind::Cross,
                    table: tref,
                    on: None,
                    using_cols: None,
                    natural: false,
                }),
            }
        }
        Ok(Some(out))
    }

    /// The column NAMES a set-returning function declares: `RETURNS TABLE(id int,
    /// v text)` names them; a `SETOF <scalar>` is one column named after the
    /// function.
    fn setof_declared_columns(
        &self,
        name: &str,
    ) -> Result<alloc::vec::Vec<alloc::string::String>, EngineError> {
        let cat = self.active_catalog();
        let overloads = cat.functions_named(name);
        let def = overloads.first().ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("function {name} does not exist"))
        })?;
        let declared = def.returns.trim();
        let upper = declared.to_ascii_uppercase();
        if upper.starts_with("TABLE(") {
            let raw = &declared["TABLE(".len()..declared.len() - 1];
            return Ok(raw
                .split(',')
                .map(|d| d.split_whitespace().next().unwrap_or("col").to_string())
                .collect());
        }
        Ok(alloc::vec![name.to_string()])
    }
}

/// A bare `TableRef` with a name — the FROM item a lowered record expansion adds.
/// v7.39 (round 205, JSON_TABLE) — the static output schema of a
/// COLUMNS list (data-independent), NESTED children inlined in
/// declaration order (PG's flattened output shape).
/// v7.39 (round 205) — pub(crate) shim so join.rs infers a wrapped
/// correlated JSON_TABLE's static schema without evaluating its doc.
pub(crate) fn json_table_schema_pub(
    cols: &[spg_sql::ast::JsonTableColumn],
) -> alloc::vec::Vec<ColumnSchema> {
    json_table_schema(cols)
}

fn json_table_schema(cols: &[spg_sql::ast::JsonTableColumn]) -> alloc::vec::Vec<ColumnSchema> {
    use spg_sql::ast::JsonTableColumn as C;
    let mut out = alloc::vec::Vec::new();
    for c in cols {
        match c {
            C::Ordinality { name } => {
                out.push(ColumnSchema::new(name.clone(), DataType::BigInt, false));
            }
            C::Regular {
                name, ty, exists, ..
            } => {
                let dt = if *exists {
                    DataType::Bool
                } else {
                    crate::conversions::column_type_to_data_type(*ty)
                };
                out.push(ColumnSchema::new(name.clone(), dt, true));
            }
            C::Nested { columns, .. } => out.extend(json_table_schema(columns)),
        }
    }
    out
}

/// v7.39 (round 205) — coerce a DEFAULT / literal value to a
/// JSON_TABLE column's declared type (the DEFAULT expr may be a
/// string literal like `'none'` that must land as the column type).
fn coerce_json_table_default(
    v: Value<'static>,
    ty: spg_sql::ast::ColumnTypeName,
    name: &str,
) -> Result<Value<'static>, EngineError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let dt = crate::conversions::column_type_to_data_type(ty);
    crate::conversions::coerce_value(v, dt, name, 0)
}

/// v7.39 (round 205) — a runtime Value → JsonValue for PASSING vars.
fn value_to_json_value(v: &Value<'_>) -> crate::json::JsonValue {
    use crate::json::JsonValue as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::SmallInt(n) => J::Number(f64::from(*n)),
        Value::Int(n) => J::Number(f64::from(*n)),
        Value::BigInt(n) => J::Number(*n as f64),
        Value::Float(x) => J::Number(*x),
        Value::Json(s) => crate::json::parse_doc(s).unwrap_or(J::Null),
        other => J::String(crate::eval::value_to_text(other)),
    }
}

fn bare_table_ref_named(name: &str) -> TableRef {
    TableRef {
        name: name.to_string(),
        alias: None,
        only: false,
        as_of_segment: None,
        unnest_expr: None,
        unnest_column_aliases: alloc::vec::Vec::new(),
        with_ordinality: false,
        generate_series_args: None,
        lateral_subquery: None,
        jsonb_each_text_arg: None,
        table_fn_call: None,
        rows_from: None,
        json_table: None,
        scalar_fn_item: false,
    }
}

impl Engine {
    /// v7.39 (read01 round 74) — run a `ROWS FROM (…)` list. Each entry yields its
    /// own rows; they zip in lockstep and a short one pads with NULL. `__array`
    /// entries are the array-able SRFs, already lowered by the parser into their
    /// scalar array form.
    fn rows_from_rows(
        &self,
        primary: &TableRef,
    ) -> Result<(alloc::vec::Vec<Row<'static>>, alloc::vec::Vec<ColumnSchema>), EngineError> {
        let entries = primary
            .rows_from
            .as_ref()
            .expect("caller guards rows_from.is_some()");
        let empty: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = self.ev_ctx(&empty, None);
        let dummy = Row::new(alloc::vec::Vec::new());
        let mut lists: alloc::vec::Vec<alloc::vec::Vec<Value<'static>>> = alloc::vec::Vec::new();
        let mut cols: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        for (name, args) in entries {
            let (vals, colname) = if name == "__array" {
                // The parser lowered this one to `<array expr>`; its rows are the
                // array's elements.
                let arr = eval::eval_expr(&args[0], &dummy, &ctx).map_err(EngineError::Eval)?;
                (
                    array_value_to_elements(&arr)?,
                    alloc::string::String::from("unnest"),
                )
            } else {
                let call = spg_sql::ast::Expr::FunctionCall {
                    name: name.clone(),
                    args: args.clone(),
                };
                (self.srf_values(&call, &dummy, &ctx)?, name.clone())
            };
            let ty = vals
                .first()
                .and_then(spg_storage::Value::data_type)
                .unwrap_or(DataType::Text);
            cols.push(ColumnSchema::new(colname, ty, true));
            lists.push(vals);
        }
        let n = lists.iter().map(alloc::vec::Vec::len).max().unwrap_or(0);
        let mut rows: alloc::vec::Vec<Row<'static>> = alloc::vec::Vec::with_capacity(n);
        for k in 0..n {
            let mut vals: alloc::vec::Vec<Value<'static>> =
                alloc::vec::Vec::with_capacity(lists.len() + 1);
            for l in &lists {
                vals.push(l.get(k).cloned().unwrap_or(Value::Null));
            }
            rows.push(Row::new(vals));
        }
        if primary.with_ordinality {
            cols.push(ColumnSchema::new(
                "ordinality".to_string(),
                DataType::BigInt,
                false,
            ));
            rows = rows
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut v = r.values;
                    v.push(Value::BigInt(i as i64 + 1));
                    Row::new(v)
                })
                .collect();
        }
        Ok((rows, cols))
    }
}

/// v7.39 (round 232) — PG names the offending set operation in its
/// arity / type-mismatch messages ("each UNION query must have the same
/// number of columns"). `UNION ALL` is still spelled UNION there.
fn set_op_name(kind: UnionKind) -> &'static str {
    match kind {
        UnionKind::All | UnionKind::Distinct => "UNION",
        UnionKind::Intersect | UnionKind::IntersectAll => "INTERSECT",
        UnionKind::Except | UnionKind::ExceptAll => "EXCEPT",
    }
}

/// v7.39 (round 233) — which output columns of a branch are PG's `unknown`
/// type: a bare string or NULL literal that no context has typed yet. SPG
/// has no `Unknown` DataType (both describe as TEXT), so the witness has to
/// be the syntax. A wildcard or a non-literal expression is never unknown.
fn branch_unknown_mask(stmt: &SelectStatement) -> Vec<bool> {
    stmt.items
        .iter()
        .map(|item| match item {
            SelectItem::Expr { expr, .. } => matches!(
                expr,
                Expr::Literal(spg_sql::ast::Literal::String(_))
                    | Expr::Literal(spg_sql::ast::Literal::Null)
            ),
            _ => false,
        })
        .collect()
}

/// v7.39 (round 233) — retype one branch column's cells, reporting the
/// conversion failure the way PG does rather than leaving the column
/// half-converted. Used when the other branch typed an untyped literal.
fn coerce_branch_column(
    rows: &mut [Row<'static>],
    col_idx: usize,
    target: DataType,
    col_name: &str,
) -> Result<(), EngineError> {
    for row in rows.iter_mut() {
        let Some(slot) = row.values.get_mut(col_idx) else {
            continue;
        };
        if matches!(slot, Value::Null) {
            continue;
        }
        *slot = crate::conversions::coerce_value(slot.clone(), target, col_name, col_idx)?;
    }
    Ok(())
}

/// v7.39 (round 727) — PG-style pull-up of a SIMPLE derived table:
/// `SELECT … FROM (SELECT <bare columns> FROM t [WHERE …]) q …`
/// rewrites to `SELECT …' FROM t [WHERE inner AND outer'] …` with every
/// reference to q's output columns substituted by the underlying column.
///
/// Admission is deliberately narrow — anything that changes cardinality,
/// order, or scope stays on the materialising path:
/// * outer: no CTEs / unions / DISTINCT [ON] / windows, single derived
///   FROM with no ordinality or positional column aliases, and no
///   subquery anywhere its expressions (an inner scope could reference
///   q too — descending is a later knife);
/// * inner: one stored table, bare-column projection only, no
///   CTE/union/DISTINCT/GROUP/HAVING/ORDER/LIMIT/OFFSET/windows/locking;
/// * every outer column reference must resolve inside q's output list —
///   a name that does not is an ERROR today, and flattening would
///   silently legalise it against the base table.
fn try_flatten_derived(stmt: &SelectStatement, primary: &TableRef) -> Option<SelectStatement> {
    use spg_sql::ast::SelectItem;
    let inner = primary.lateral_subquery.as_deref()?;
    // Outer shape.
    if !stmt.ctes.is_empty()
        || !stmt.unions.is_empty()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || !stmt.window_check_exprs.is_empty()
        || stmt.locking.is_some()
        || primary.with_ordinality
        || !primary.unnest_column_aliases.is_empty()
    {
        return None;
    }
    // Inner shape.
    if !inner.ctes.is_empty()
        || !inner.unions.is_empty()
        || inner.distinct
        || !inner.distinct_on.is_empty()
        || inner.group_by.is_some()
        || inner.group_by_all
        || inner.having.is_some()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
        || inner.offset.is_some()
        || !inner.window_check_exprs.is_empty()
        || inner.locking.is_some()
    {
        return None;
    }
    let ifrom = inner.from.as_ref()?;
    let it = &ifrom.primary;
    if !ifrom.joins.is_empty()
        || it.name.is_empty()
        || it.lateral_subquery.is_some()
        || it.unnest_expr.is_some()
        || it.generate_series_args.is_some()
        || it.as_of_segment.is_some()
        || it.jsonb_each_text_arg.is_some()
        || it.table_fn_call.is_some()
        || it.rows_from.is_some()
        || it.json_table.is_some()
        || it.with_ordinality
        || !it.unnest_column_aliases.is_empty()
    {
        return None;
    }
    if inner.where_.as_ref().is_some_and(crate::expr_has_subquery) {
        return None;
    }
    // The output map: q's visible name -> the underlying column.
    let inner_alias = it.alias.clone().unwrap_or_else(|| it.name.clone());
    let mut map: alloc::collections::BTreeMap<String, spg_sql::ast::ColumnName> =
        alloc::collections::BTreeMap::new();
    for item in &inner.items {
        let SelectItem::Expr { expr, alias } = item else {
            return None;
        };
        let Expr::Column(c) = expr else {
            return None;
        };
        if let Some(q) = c.qualifier.as_deref()
            && !q.eq_ignore_ascii_case(&inner_alias)
        {
            return None;
        }
        let out_name = alias.clone().unwrap_or_else(|| c.name.clone());
        // A duplicated output name would make substitution ambiguous.
        if map
            .insert(out_name.to_ascii_lowercase(), c.clone())
            .is_some()
        {
            return None;
        }
    }
    if map.is_empty() {
        return None;
    }
    let derived_alias = primary
        .alias
        .clone()
        .unwrap_or_else(|| primary.name.clone())
        .to_ascii_lowercase();
    // Substitute in a clone; bail (None) on the first reference the map
    // cannot answer.
    let mut out = stmt.clone();
    let ok = core::cell::Cell::new(true);
    let mut subst = |e: &mut Expr| -> bool {
        match e {
            Expr::Column(c) => {
                match c.qualifier.as_deref() {
                    Some(q) if q.eq_ignore_ascii_case(&derived_alias) => {}
                    None => {}
                    Some(_) => {
                        ok.set(false);
                        return true;
                    }
                }
                match map.get(&c.name.to_ascii_lowercase()) {
                    Some(target) => *c = target.clone(),
                    None => ok.set(false),
                }
                true
            }
            // Any subquery could reference q from its own scope;
            // descending is a later knife — bail for now.
            Expr::ScalarSubquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::RowInSubquery { .. }
            | Expr::RowCmpSubquery { .. } => {
                ok.set(false);
                true
            }
            _ => false,
        }
    };
    for item in &mut out.items {
        match item {
            SelectItem::Expr { expr, .. } => {
                crate::expr_analysis::rewrite_nodes_mut(expr, &mut subst);
            }
            // `SELECT * FROM (…) q` means q's columns, in q's order.
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => return None,
        }
    }
    if let Some(w) = &mut out.where_ {
        crate::expr_analysis::rewrite_nodes_mut(w, &mut subst);
    }
    if let Some(gs) = &mut out.group_by {
        for g in gs {
            crate::expr_analysis::rewrite_nodes_mut(g, &mut subst);
        }
    }
    if let Some(h) = &mut out.having {
        crate::expr_analysis::rewrite_nodes_mut(h, &mut subst);
    }
    for o in &mut out.order_by {
        crate::expr_analysis::rewrite_nodes_mut(&mut o.expr, &mut subst);
    }
    for d in &mut out.distinct_on {
        crate::expr_analysis::rewrite_nodes_mut(d, &mut subst);
    }
    if !ok.get() {
        return None;
    }
    // FROM becomes the stored table; the filters conjoin.
    out.from = Some(spg_sql::ast::FromClause {
        primary: it.clone(),
        joins: Vec::new(),
    });
    out.where_ = match (inner.where_.clone(), out.where_.take()) {
        (Some(a), Some(b)) => Some(Expr::Binary {
            lhs: alloc::boxed::Box::new(a),
            op: spg_sql::ast::BinOp::And,
            rhs: alloc::boxed::Box::new(b),
        }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    };
    Some(out)
}

/// v7.39 (round 742) — rewrite `SELECT count(*) FROM (SELECT <plain>
/// FROM t [WHERE p] ORDER BY … OFFSET k [no LIMIT]) q` into
/// `SELECT greatest(count(*) - k, 0) FROM t [WHERE p]`. Sound because
/// ORDER BY is count-invariant and OFFSET k drops exactly min(k, n)
/// rows. Admission mirrors the flatten's conservatism; a LIMIT, a
/// DISTINCT, an SRF, or an unprovable inner shape stays put.
fn try_count_over_offset(stmt: &SelectStatement, primary: &TableRef) -> Option<SelectStatement> {
    use spg_sql::ast::{Expr as E, LimitExpr, SelectItem};
    let inner = primary.lateral_subquery.as_deref()?;
    // Outer: exactly `SELECT count(*)`, nothing else.
    if !stmt.ctes.is_empty()
        || !stmt.unions.is_empty()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || stmt.where_.is_some()
        || stmt.group_by.is_some()
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || stmt.items.len() != 1
    {
        return None;
    }
    let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
        return None;
    };
    let E::FunctionCall { name, args } = expr else {
        return None;
    };
    if !name.eq_ignore_ascii_case("count_star") || !args.is_empty() {
        return None;
    }
    // Inner: flatten-shaped plus ORDER BY and a literal OFFSET, no LIMIT.
    let Some(LimitExpr::Literal(k)) = &inner.offset else {
        return None;
    };
    let k = i64::from(*k);
    if inner.limit.is_some() || inner.order_by.is_empty() {
        return None;
    }
    let mut counted = inner.clone();
    counted.order_by = Vec::new();
    counted.offset = None;
    // The stripped inner must now be a provable simple shape (its
    // items become irrelevant — count(*) reads none of them — but an
    // SRF item would change the row count, so the flatten predicate's
    // scrutiny still applies).
    let base = matview_flatten_probe(&counted)?;
    let mut out = stmt.clone();
    out.items = alloc::vec![SelectItem::Expr {
        expr: E::FunctionCall {
            name: String::from("greatest"),
            args: alloc::vec![
                E::Binary {
                    lhs: alloc::boxed::Box::new(E::FunctionCall {
                        name: String::from("count_star"),
                        args: alloc::vec![],
                    }),
                    op: spg_sql::ast::BinOp::Sub,
                    rhs: alloc::boxed::Box::new(E::Literal(spg_sql::ast::Literal::Integer(k))),
                },
                E::Literal(spg_sql::ast::Literal::Integer(0)),
            ],
        },
        alias: Some(String::from("count")),
    }];
    out.from = Some(spg_sql::ast::FromClause {
        primary: base,
        joins: Vec::new(),
    });
    out.where_ = counted.where_.clone();
    Some(out)
}

/// The inner-shape probe `try_count_over_offset` shares with the
/// flatten: single stored table, no modifiers, no subqueries, no SRF
/// items. Returns the base TableRef.
fn matview_flatten_probe(inner: &SelectStatement) -> Option<TableRef> {
    use spg_sql::ast::SelectItem;
    if !inner.ctes.is_empty()
        || !inner.unions.is_empty()
        || inner.distinct
        || !inner.distinct_on.is_empty()
        || inner.group_by.is_some()
        || inner.group_by_all
        || inner.having.is_some()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
        || inner.offset.is_some()
        || !inner.window_check_exprs.is_empty()
        || inner.locking.is_some()
    {
        return None;
    }
    let ifrom = inner.from.as_ref()?;
    let it = &ifrom.primary;
    if !ifrom.joins.is_empty()
        || it.name.is_empty()
        || it.lateral_subquery.is_some()
        || it.unnest_expr.is_some()
        || it.generate_series_args.is_some()
        || it.as_of_segment.is_some()
        || it.jsonb_each_text_arg.is_some()
        || it.table_fn_call.is_some()
        || it.rows_from.is_some()
        || it.json_table.is_some()
        || it.with_ordinality
    {
        return None;
    }
    for item in &inner.items {
        match item {
            SelectItem::Expr { expr, .. } => {
                if crate::expr_has_subquery(expr) || expr_contains_builtin_srf(expr) {
                    return None;
                }
            }
            SelectItem::Wildcard => {}
            SelectItem::QualifiedWildcard(_) => return None,
        }
    }
    if inner.where_.as_ref().is_some_and(crate::expr_has_subquery) {
        return None;
    }
    Some(it.clone())
}

/// v7.39 (round 743) — rewrite `SELECT count(*) FROM (SELECT
/// unnest(ARRAY[e1..ek]) [AS v] FROM t [WHERE p]) q` into
/// `SELECT count(*) * k FROM t [WHERE p]`. Sound because a
/// constant-LENGTH array literal unnests to exactly k rows per input
/// row (NULL elements are rows too). One SRF item only, elements
/// subquery-free, and the stripped inner must pass the same probe the
/// count-over-offset rewrite uses.
fn try_count_over_const_unnest(
    stmt: &SelectStatement,
    primary: &TableRef,
) -> Option<SelectStatement> {
    use spg_sql::ast::{Expr as E, SelectItem};
    let inner = primary.lateral_subquery.as_deref()?;
    if !stmt.ctes.is_empty()
        || !stmt.unions.is_empty()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || stmt.where_.is_some()
        || stmt.group_by.is_some()
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || stmt.items.len() != 1
    {
        return None;
    }
    let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
        return None;
    };
    let E::FunctionCall { name, args } = expr else {
        return None;
    };
    if !name.eq_ignore_ascii_case("count_star") || !args.is_empty() {
        return None;
    }
    // Inner: exactly one item, and it is unnest(ARRAY[...]).
    if inner.items.len() != 1
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
        || inner.offset.is_some()
    {
        return None;
    }
    let SelectItem::Expr { expr: item, .. } = &inner.items[0] else {
        return None;
    };
    let E::FunctionCall {
        name: fname,
        args: fargs,
    } = item
    else {
        return None;
    };
    if !fname.eq_ignore_ascii_case("unnest") || fargs.len() != 1 {
        return None;
    }
    let E::Array(elems) = &fargs[0] else {
        return None;
    };
    if elems.is_empty() || elems.iter().any(crate::expr_has_subquery) {
        return None;
    }
    let k = elems.len() as i64;
    // The stripped inner (the SRF item replaced by a plain constant)
    // must be the provable simple shape.
    let mut counted = inner.clone();
    counted.items = alloc::vec![SelectItem::Expr {
        expr: E::Literal(spg_sql::ast::Literal::Integer(1)),
        alias: None,
    }];
    let base = matview_flatten_probe(&counted)?;
    let mut out = stmt.clone();
    out.items = alloc::vec![SelectItem::Expr {
        expr: E::Binary {
            lhs: alloc::boxed::Box::new(E::FunctionCall {
                name: String::from("count_star"),
                args: alloc::vec![],
            }),
            op: spg_sql::ast::BinOp::Mul,
            rhs: alloc::boxed::Box::new(E::Literal(spg_sql::ast::Literal::Integer(k))),
        },
        alias: Some(String::from("count")),
    }];
    out.from = Some(spg_sql::ast::FromClause {
        primary: base,
        joins: Vec::new(),
    });
    out.where_ = counted.where_.clone();
    Some(out)
}
