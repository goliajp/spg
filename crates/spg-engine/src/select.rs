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
    ByteBudget, CancelToken, Engine, EngineError, QueryResult, aggregate, apply_offset_and_limit,
    apply_offset_and_limit_tagged, approx_row_bytes, build_order_keys, collect_meta_view_names,
    collect_qualified_refs, collect_scalar_subqueries, collect_window_nodes,
    compute_window_partition, eval, expr_tree_has_subquery, materialise_in_order,
    materialise_meta_view, memoize, order_by_value_cmp, order_key_cmp, partial_sort_tagged,
    partition_key_cmp, rewrite_window_to_columns, select_has_window, select_references_meta_view,
    select_refers_to, sort_by_keys, synth_info_key_column_usage,
    synth_info_referential_constraints, synth_info_routines, synth_info_statistics,
    synth_information_schema_columns, synth_information_schema_tables, synth_mysql_db,
    synth_mysql_user, synth_pg_attribute, synth_pg_class, synth_pg_constraint, synth_pg_database,
    synth_pg_extension, synth_pg_index_raw, synth_pg_indexes, synth_pg_namespace, synth_pg_proc,
    synth_pg_roles, synth_pg_settings, synth_pg_trigger, synth_pg_type, synth_pg_views,
    try_gin_seek, try_index_seek, try_nsw_knn, try_trgm_seek, value_is_integer, value_to_i64,
};

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
        let filtered: Vec<Row>;
        if from.joins.is_empty() {
            let primary = &from.primary;
            let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
                StorageError::TableNotFound {
                    name: primary.name.clone(),
                }
            })?;
            let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
            schema_cols_owned = table.schema().columns.clone();
            alias_opt = Some(alias);
            // Materialise WHERE-filtered rows owned so the JOIN
            // and single-table paths share a single downstream
            // shape. The clone is cheap relative to the window
            // computation that follows.
            let ctx = self.ev_ctx(&schema_cols_owned, alias_opt);
            let mut owned: Vec<Row> = Vec::new();
            for (i, row) in table.rows().iter().enumerate() {
                if i.is_multiple_of(256) {
                    cancel.check()?;
                }
                if let Some(w) = &stmt.where_ {
                    let cond = eval::eval_expr(w, row, &ctx)?;
                    if !matches!(cond, Value::Bool(true)) {
                        continue;
                    }
                }
                owned.push(row.clone());
            }
            filtered = owned;
        } else {
            let deferred = self.build_joined_filtered_rows(
                from,
                stmt.where_.as_ref(),
                cancel,
                None,
                &mut ByteBudget::new(self.max_query_bytes),
            )?;
            // Window path needs owned Rows; materialise the survivors
            // before moving out the schema.
            filtered = deferred.materialise();
            schema_cols_owned = deferred.combined_schema;
            alias_opt = None;
        }
        let schema_cols = &schema_cols_owned;
        let ctx = self.ev_ctx(schema_cols, alias_opt);
        let alias = alias_opt.unwrap_or("");
        let n_rows = filtered.len();
        // Borrow refs into the owned row vec once so the downstream
        // `compute_window_partition` call (which takes `&[&Row]`) and
        // the per-row eval loops share a single backing buffer.
        let filtered_refs: Vec<&Row> = filtered.iter().collect();

        // 2) Collect unique window function nodes from projection.
        let mut window_nodes: Vec<Expr> = Vec::new();
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_window_nodes(expr, &mut window_nodes);
            }
        }

        // 3) For each window, compute per-row value.
        // Index: same order as window_nodes; for row i, win_vals[w][i].
        let mut win_vals: Vec<Vec<Value>> = Vec::with_capacity(window_nodes.len());
        for wnode in &window_nodes {
            let Expr::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
                null_treatment,
            } = wnode
            else {
                unreachable!("collect_window_nodes pushes only WindowFunction");
            };
            // Compute (partition_key, order_key, original_index) for each row.
            let mut indexed: Vec<(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)> =
                Vec::with_capacity(n_rows);
            for (i, row) in filtered.iter().enumerate() {
                let pkey: Vec<Value> = partition_by
                    .iter()
                    .map(|p| eval::eval_expr(p, row, &ctx))
                    .collect::<Result<_, _>>()?;
                let okey: Vec<(Value, bool, Option<bool>)> = order_by
                    .iter()
                    .map(|(e, desc, nf)| eval::eval_expr(e, row, &ctx).map(|v| (v, *desc, *nf)))
                    .collect::<Result<_, _>>()?;
                indexed.push((pkey, okey, i));
            }
            // Sort by (partition_key, order_key). Partition key uses
            // a stable encoded form; order key respects ASC/DESC.
            indexed.sort_by(|a, b| {
                let p_cmp = partition_key_cmp(&a.0, &b.0);
                if p_cmp != core::cmp::Ordering::Equal {
                    return p_cmp;
                }
                order_key_cmp(&a.1, &b.1)
            });
            // Per-partition compute.
            let mut out_vals: Vec<Value> = alloc::vec![Value::Null; n_rows];
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
                    !order_by.is_empty(),
                    frame.as_ref(),
                    *null_treatment,
                    &indexed[p_start..p_end],
                    &filtered_refs,
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
        // 5) Build extended rows: each row gets its window values appended.
        let mut ext_rows: Vec<Row> = Vec::with_capacity(n_rows);
        for i in 0..n_rows {
            let mut values = filtered[i].values.clone();
            for w in 0..window_nodes.len() {
                values.push(win_vals[w][i].clone());
            }
            ext_rows.push(Row::new(values));
        }
        // 6) Rewrite the projection: WindowFunction nodes → Column(__win_N).
        let mut rewritten_items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
        for item in &stmt.items {
            let new_item = match item {
                SelectItem::Wildcard => SelectItem::Wildcard,
                SelectItem::Expr { expr, alias } => {
                    let mut e = expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    SelectItem::Expr {
                        expr: e,
                        alias: alias.clone(),
                    }
                }
            };
            rewritten_items.push(new_item);
        }

        // 7) Project into final rows. JOIN case uses None so the
        // qualifier check in `resolve_column` falls through to the
        // composite `alias.col` schema lookup; single-table case
        // keeps the bare alias so `bare_col` resolution still
        // works for the projection's per-row column references.
        let ext_ctx = EvalContext::new(&ext_cols, alias_opt);
        let projection = build_projection(&rewritten_items, &ext_cols, alias)?;
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::with_capacity(n_rows);
        for (i, row) in ext_rows.iter().enumerate() {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
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
                    keys.push(value_to_order_key(&key)?);
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
        let mut out_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        apply_offset_and_limit(&mut out_rows, stmt.offset_literal(), stmt.limit_literal());
        let final_cols: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
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
        let mut needed: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        collect_meta_view_names(stmt, &mut needed);
        let mut catalog = self.active_catalog().clone();
        for view in &needed {
            if catalog.get(view).is_some() {
                continue;
            }
            match view.as_str() {
                "__spg_info_columns" => {
                    let (schema, rows) = synth_information_schema_columns(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_info_tables" => {
                    let (schema, rows) = synth_information_schema_tables(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_class" => {
                    let (schema, rows) = synth_pg_class(self.active_catalog());
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
                // v7.17.0 Phase 3.P0-53 — pg_catalog.pg_indexes view
                // for pgAdmin / DataGrip "indexes per table" listings.
                "__spg_pg_indexes" => {
                    let (schema, rows) = synth_pg_indexes(self.active_catalog());
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
                // v7.17.0 Phase 3.P0-55 — pg_catalog.pg_database /
                // pg_roles / pg_user. SPG is single-database so
                // pg_database surfaces just `postgres`; pg_roles
                // / pg_user walk the engine's UserStore.
                "__spg_pg_database" => {
                    let (schema, rows) = synth_pg_database(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_roles" | "__spg_pg_user" => {
                    let (schema, rows) = synth_pg_roles(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-56 — pg_catalog.pg_views. PG's
                // pg_views surfaces every CREATE VIEW result; SPG
                // ships one row per declared view from the catalog.
                "__spg_pg_views" => {
                    let (schema, rows) = synth_pg_views(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-56 — pg_catalog.pg_matviews.
                // SPG has no materialised view surface yet so the
                // table shares pg_views's schema but stays empty.
                "__spg_pg_matviews" => {
                    let (schema, _) = synth_pg_views(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, Vec::new())?;
                }
                // pg_catalog.pg_extension — native capability list
                // (mailrs embed round-12).
                "__spg_pg_extension" => {
                    let (schema, rows) = synth_pg_extension();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-57 — pg_catalog.pg_settings.
                "__spg_pg_settings" => {
                    let (schema, rows) = synth_pg_settings(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-63 — information_schema.KEY_COLUMN_USAGE.
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
                // v7.17.0 Phase 3.P0-65 — mysql.user / mysql.db.
                "__spg_mysql_user" => {
                    let (schema, rows) = synth_mysql_user(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_mysql_db" => {
                    let (schema, rows) = synth_mysql_db();
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
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        temp.meta_views_materialised = true;
        temp.exec_select_cancel(stmt, cancel)
    }

    pub(crate) fn exec_with_ctes(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut catalog = self.active_catalog().clone();
        for cte in &stmt.ctes {
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let (columns, rows) = if cte.recursive {
                self.materialise_recursive_cte(cte, &catalog, cancel)?
            } else {
                // v7.25 (round-17) — run the body against the
                // ACCUMULATED catalog so a CTE can reference every
                // CTE declared before it (`WITH a AS (…), b AS
                // (SELECT … FROM a)`). Executing on `self` lost the
                // already-materialised CTE tables.
                let mut cte_engine = Engine::restore(catalog.clone());
                if let Some(c) = self.clock {
                    cte_engine = cte_engine.with_clock(c);
                }
                if let Some(f) = self.salt_fn {
                    cte_engine = cte_engine.with_salt_fn(f);
                }
                let body_result = cte_engine.exec_select_cancel(&cte.body, cancel)?;
                let QueryResult::Rows { columns, rows } = body_result else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} body did not return rows",
                        cte.name
                    )));
                };
                (columns, rows)
            };
            // v4.22: the projection builder labels any non-column
            // expression as Text — including literal SELECT 1.
            // Promote each column's type to whatever the rows
            // actually carry so the CTE storage table accepts them.
            let inferred = infer_column_types(&columns, &rows);
            let mut columns = inferred;
            // v4.22: apply optional `WITH name(a, b, c)` overrides.
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
    fn materialise_recursive_cte(
        &self,
        cte: &spg_sql::ast::Cte,
        base_catalog: &Catalog,
        cancel: CancelToken<'_>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row>), EngineError> {
        const MAX_TOTAL_ROWS: usize = 1_000_000;
        const MAX_ITERATIONS: usize = 100_000;
        cancel.check()?;
        if cte.body.unions.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?} body must be a UNION of an anchor and a recursive term",
                cte.name
            )));
        }
        // Anchor: the body's leading SELECT, with unions stripped.
        let mut anchor = cte.body.clone();
        let union_terms = core::mem::take(&mut anchor.unions);
        anchor.ctes = Vec::new();
        // Anchor must not reference the CTE name.
        if select_refers_to(&anchor, &cte.name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: the anchor must not reference the CTE itself",
                cte.name
            )));
        }
        let anchor_result = self.exec_select_cancel(&anchor, cancel)?;
        let QueryResult::Rows {
            columns: anchor_cols,
            rows: anchor_rows,
        } = anchor_result
        else {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: anchor did not return rows",
                cte.name
            )));
        };
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
        let mut all_rows: Vec<Row> = anchor_rows.clone();
        let mut working_set: Vec<Row> = anchor_rows;
        let mut seen: alloc::collections::BTreeSet<Vec<u8>> = alloc::collections::BTreeSet::new();
        // Track at least one "all UNION ALL" flag — if every union
        // kind is ALL we skip the dedup step (faster + matches PG).
        let all_union_all = union_terms.iter().all(|(k, _)| matches!(k, UnionKind::All));
        if !all_union_all {
            for r in &all_rows {
                seen.insert(encode_row_key(r));
            }
        }
        for iter in 0..MAX_ITERATIONS {
            cancel.check()?;
            if working_set.is_empty() {
                break;
            }
            // Build a fresh catalog: base + CTE bound to working_set.
            let mut iter_catalog = base_catalog.clone();
            let schema = TableSchema::new(cte.name.clone(), columns.clone());
            iter_catalog
                .create_table(schema)
                .map_err(EngineError::Storage)?;
            {
                let table = iter_catalog.get_mut(&cte.name).expect("just-created");
                for row in &working_set {
                    table.insert(row.clone()).map_err(EngineError::Storage)?;
                }
            }
            let mut iter_engine = Engine::restore(iter_catalog);
            if let Some(c) = self.clock {
                iter_engine = iter_engine.with_clock(c);
            }
            if let Some(f) = self.salt_fn {
                iter_engine = iter_engine.with_salt_fn(f);
            }
            // Run each recursive term in sequence and collect new rows.
            let mut next_set: Vec<Row> = Vec::new();
            for (_, term) in &union_terms {
                let mut term = term.clone();
                term.ctes = Vec::new();
                let r = iter_engine.exec_select_cancel(&term, cancel)?;
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
            if let SelectItem::Expr { expr, .. } = item {
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
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
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
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                self.resolve_expr_subqueries(array, cancel)?;
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
        row: &Row,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        alias: &str,
    ) -> Result<Row, EngineError> {
        let ctx = EvalContext::new(schema_cols, Some(alias));
        let cancel = CancelToken::none();
        let mut out_vals = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard => {
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
                SelectItem::Wildcard => {
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
                        out.push(ColumnSchema::new(name, sc.ty, sc.nullable));
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
                    let (ty, nullable) =
                        build_projection(core::slice::from_ref(item), schema_cols, table_alias)
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
    pub(crate) fn exec_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v7.17.0 Phase 1.2 — user-defined VIEW expansion. If the
        // FROM / JOIN graph references any catalogued view name,
        // re-parse the view body and prepend it as a synthetic
        // CTE. Recurses on views-in-views via the regular CTE
        // dispatch below. Fast-path: skip the walker entirely when
        // the catalog has no views (the typical OLTP load).
        if !self.active_catalog().views().is_empty() {
            if let Some(rewritten) = self.expand_views_in_select(stmt)? {
                return self.exec_select_cancel(&rewritten, cancel);
            }
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
        if let Some(from) = &stmt.from
            && from.joins.is_empty()
            && stmt.where_.is_none()
            && stmt.group_by.is_none()
            && stmt.having.is_none()
            && stmt.unions.is_empty()
            && stmt.order_by.is_empty()
            && stmt.limit.is_none()
            && stmt.offset.is_none()
            && !stmt.distinct
            && stmt.items.iter().all(|i| matches!(i, SelectItem::Wildcard))
        {
            let lower = from.primary.name.to_ascii_lowercase();
            match lower.as_str() {
                "spg_statistic" => return Ok(self.exec_spg_statistic()),
                // v6.5.0 — observability v2 virtual tables.
                "spg_stat_replication" => return Ok(self.exec_spg_stat_replication()),
                "spg_stat_segment" => return Ok(self.exec_spg_stat_segment()),
                // v7.31 — memory-campaign bucket meters.
                "spg_memory_stats" => return Ok(self.exec_spg_memory_stats()),
                "spg_stat_query" => return Ok(self.exec_spg_stat_query()),
                "spg_stat_activity" => return Ok(self.exec_spg_stat_activity()),
                "spg_audit_chain" => return Ok(self.exec_spg_audit_chain()),
                "spg_audit_verify" => return Ok(self.exec_spg_audit_verify()),
                "spg_table_ddl" => return Ok(self.exec_spg_table_ddl()),
                "spg_role_ddl" => return Ok(self.exec_spg_role_ddl()),
                "spg_database_ddl" => return Ok(self.exec_spg_database_ddl()),
                _ => {}
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
            self.resolve_select_subqueries(&mut stmt_owned, cancel)?;
            &stmt_owned
        } else {
            stmt
        };
        if stmt_ref.unions.is_empty() {
            return self.exec_bare_select_cancel(stmt_ref, cancel);
        }
        // UNION path: clone-strip the head into a bare block (its own
        // DISTINCT and any inner ORDER BY are dropped by parser rule —
        // the wrapper SelectStatement carries them), execute, then chain
        // peers with left-associative dedup semantics.
        let mut head = stmt_ref.clone();
        head.unions = Vec::new();
        head.order_by = Vec::new();
        head.limit = None;
        let QueryResult::Rows { columns, mut rows } =
            self.exec_bare_select_cancel(&head, cancel)?
        else {
            unreachable!("bare SELECT cannot return CommandOk")
        };
        for (kind, peer) in &stmt_ref.unions {
            let QueryResult::Rows {
                columns: peer_cols,
                rows: peer_rows,
            } = self.exec_bare_select_cancel(peer, cancel)?
            else {
                unreachable!("bare SELECT cannot return CommandOk")
            };
            if peer_cols.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UNION arity mismatch: head has {} columns, peer has {}",
                    columns.len(),
                    peer_cols.len()
                )));
            }
            rows.extend(peer_rows);
            if matches!(kind, UnionKind::Distinct) {
                rows = dedup_rows(rows);
            }
        }
        // ORDER BY at the top of a UNION applies to the combined result.
        // Eval against the projected schema (NOT the source table).
        if !stmt.order_by.is_empty() {
            let synth_ctx = EvalContext::new(&columns, None);
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            let mut tagged: Vec<(Vec<f64>, Row)> = Vec::with_capacity(rows.len());
            for r in rows {
                let keys = build_order_keys(&stmt.order_by, &r, &synth_ctx)?;
                tagged.push((keys, r));
            }
            sort_by_keys(&mut tagged, &descs);
            rows = tagged.into_iter().map(|(_, r)| r).collect();
        }
        apply_offset_and_limit(&mut rows, stmt.offset_literal(), stmt.limit_literal());
        Ok(QueryResult::Rows { columns, rows })
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
        // Evaluate the array expression once. Empty schema / empty
        // row — uncorrelated UNNEST cannot reference outer columns.
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        // v7.11.13 — unnest dispatches per array element type so
        // INT[] / BIGINT[] surface their PG types in projection.
        let (elem_dtype, rows): (DataType, alloc::vec::Vec<Row>) =
            match eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)? {
                Value::Null => (DataType::Text, alloc::vec::Vec::new()),
                Value::TextArray(items) => {
                    let rows = items
                        .into_iter()
                        .map(|item| {
                            Row::new(alloc::vec![match item {
                                Some(s) => Value::Text(s),
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
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "unnest() expects an array argument, got {:?}",
                        other.data_type()
                    )));
                }
            };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "unnest".to_string());
        // v7.13.2 — mailrs round-6 S5. Honour PG-standard
        // `UNNEST(arr) AS p(col_name)` column-list aliasing: the
        // first entry overrides the projected column's name.
        // Without the column list, fall back to the table alias
        // (pre-v7.13.2 behaviour).
        let col_name = primary
            .unnest_column_aliases
            .first()
            .cloned()
            .unwrap_or_else(|| alias.clone());
        let col_schema = ColumnSchema::new(col_name, elem_dtype, true);
        let schema_cols = alloc::vec![col_schema.clone()];
        let scan_ctx = EvalContext::new(&schema_cols, Some(&alias));
        // Apply WHERE.
        let filtered: alloc::vec::Vec<Row> = if let Some(w) = &stmt.where_ {
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
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let filtered_refs: alloc::vec::Vec<RowRef<'_>> =
                filtered.iter().map(RowRef::Owned).collect();
            let agg = aggregate::run(
                stmt,
                &filtered_refs,
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection = build_projection(&stmt.items, &schema_cols, &alias)?;
        let mut projected_rows: alloc::vec::Vec<Row> =
            alloc::vec::Vec::with_capacity(filtered.len());
        // v7.19 P5 — Set-Returning-Function in projection
        // position (PG `SELECT unnest(arr) FROM t` shape). When a
        // SELECT item evaluates to a top-level unnest(arr) call,
        // expand it: for each input row, evaluate the array, emit
        // one output row per element, broadcasting non-SRF
        // projections from the same input row. Multi-SRF + LCM
        // padding stays a documented carve-out; mailrs uses
        // single-SRF for redirect_uris.
        let srf_position = projection.iter().position(|p| is_top_level_unnest(&p.expr));
        if let Some(srf_idx) = srf_position {
            let srf_arg = top_level_unnest_arg(&projection[srf_idx].expr)
                .expect("checked by is_top_level_unnest above");
            for row in &filtered {
                let arr_val =
                    eval::eval_expr(srf_arg, row, &scan_ctx).map_err(EngineError::Eval)?;
                let elements = array_value_to_elements(&arr_val)?;
                // Empty array → zero rows for this input row (PG
                // semantics: `SELECT unnest('{}'::int[])` returns
                // 0 rows, not a single NULL row).
                for elem in elements {
                    let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                    for (i, p) in projection.iter().enumerate() {
                        if i == srf_idx {
                            vals.push(elem.clone());
                        } else {
                            vals.push(
                                eval::eval_expr(&p.expr, row, &scan_ctx)
                                    .map_err(EngineError::Eval)?,
                            );
                        }
                    }
                    projected_rows.push(Row::new(vals));
                }
            }
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
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();
        // Re-evaluate ORDER BY against the source schema (pre-projection
        // so col refs by name still resolve through `scan_ctx`).
        if !stmt.order_by.is_empty() {
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value>)> = filtered
                .iter()
                .enumerate()
                .map(|(i, r)| -> Result<_, EngineError> {
                    let keys: Result<Vec<Value>, EngineError> = stmt
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
                    let cmp = order_by_value_cmp(o.desc, o.nulls_first, ka, kb);
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
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        let mut arg_values: alloc::vec::Vec<Value> = alloc::vec::Vec::with_capacity(args.len());
        for a in args {
            arg_values.push(eval::eval_expr(a, &dummy_row, &ctx).map_err(EngineError::Eval)?);
        }
        // Dispatch on the start value's shape. Reject mixed-shape
        // calls early (e.g. start = timestamp, stop = integer) so
        // the caller gets a clean error rather than a panic.
        let (elem_dtype, rows) = match arg_values.as_slice() {
            [Value::Timestamp(start), Value::Timestamp(stop), step] => {
                let interval_step = match step {
                    Value::Interval { .. } => step.clone(),
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "generate_series(timestamp, timestamp, …): \
                             step must be INTERVAL, got {:?}",
                            other.data_type()
                        )));
                    }
                };
                let rows = generate_series_timestamps(*start, *stop, interval_step, &cancel)?;
                (DataType::Timestamp, rows)
            }
            [start, stop, step]
                if value_is_integer(start) && value_is_integer(stop) && value_is_integer(step) =>
            {
                let s = value_to_i64(start);
                let e = value_to_i64(stop);
                let st = value_to_i64(step);
                let rows = generate_series_integers(s, e, st, &cancel)?;
                (DataType::BigInt, rows)
            }
            [start, stop] if value_is_integer(start) && value_is_integer(stop) => {
                let s = value_to_i64(start);
                let e = value_to_i64(stop);
                let rows = generate_series_integers(s, e, 1, &cancel)?;
                (DataType::BigInt, rows)
            }
            _ => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "generate_series(): v7.17 supports integer or (timestamp, timestamp, interval) \
                     argument shapes; got {:?}",
                    arg_values
                        .iter()
                        .map(|v| v.data_type())
                        .collect::<alloc::vec::Vec<_>>()
                )));
            }
        };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "generate_series".to_string());
        let col_name = alias.clone();
        let col_schema = ColumnSchema::new(col_name, elem_dtype, true);
        let schema_cols = alloc::vec![col_schema.clone()];
        let scan_ctx = EvalContext::new(&schema_cols, Some(&alias));
        // WHERE.
        let filtered: alloc::vec::Vec<Row> = if let Some(w) = &stmt.where_ {
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
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let filtered_refs: alloc::vec::Vec<RowRef<'_>> =
                filtered.iter().map(RowRef::Owned).collect();
            let agg = aggregate::run(
                stmt,
                &filtered_refs,
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
            )?;
            return self.finish_agg_result(agg, stmt, cancel);
        }
        // Projection.
        let projection = build_projection(&stmt.items, &schema_cols, &alias)?;
        let mut projected_rows: alloc::vec::Vec<Row> =
            alloc::vec::Vec::with_capacity(filtered.len());
        let mut proj_memo = memoize::MemoizeCache::default();
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
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();
        // ORDER BY against the source schema.
        if !stmt.order_by.is_empty() {
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value>)> = filtered
                .iter()
                .enumerate()
                .map(|(i, r)| -> Result<_, EngineError> {
                    let keys: Result<Vec<Value>, EngineError> = stmt
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
                    let cmp = order_by_value_cmp(o.desc, o.nulls_first, ka, kb);
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
        // v7.16.2 — same meta-view dispatch as
        // `exec_select_cancel`, applied here too because
        // `subquery_replacement` enters this function directly
        // for Exists / ScalarSubquery / InSubquery resolution
        // (bypassing the top-level entry to avoid double
        // subquery walking). Without this dispatch the subquery
        // hits `__spg_info_columns` and reports TableNotFound.
        if !self.meta_views_materialised && select_references_meta_view(stmt) {
            return self.exec_select_with_meta_views(stmt, cancel);
        }
        // v4.12: window-function path. When the projection contains
        // any `name(args) OVER (...)` we route to the dedicated
        // executor — partition + sort + per-row window value before
        // the regular projection.
        if select_has_window(stmt) {
            return self.exec_select_with_window(stmt, cancel);
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
        if !from.joins.is_empty() {
            return self.exec_joined_select(stmt, from, cancel);
        }
        // v7.11.7 — `FROM unnest(<expr>) [AS] <alias>`. Synthesise a
        // single-column table at SELECT entry by evaluating the
        // expression once against the empty row (UNNEST is
        // uncorrelated in v7.11; correlated / LATERAL unnest is a
        // v7.12 carve-out). Build a virtual `Table` in a heap-only
        // catalog, then route to the regular scan path.
        if from.primary.unnest_expr.is_some() {
            return self.exec_select_unnest(stmt, &from.primary, cancel);
        }
        // v7.17.0 Phase 3.10 — `FROM generate_series(start, stop
        // [, step])` set-returning source. Dispatch mirrors UNNEST:
        // materialise the row stream from a single eval pass, then
        // run the regular projection / WHERE / ORDER BY / LIMIT
        // pipeline over the synthetic single-column table.
        if from.primary.generate_series_args.is_some() {
            return self.exec_select_generate_series(stmt, &from.primary, cancel);
        }
        let primary = &from.primary;
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let schema_cols = &table.schema().columns;
        // The qualifier accepted on column refs is the alias (if any) else the
        // bare table name.
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let ctx = self.ev_ctx(schema_cols, Some(alias));

        // NSW kNN planner: `ORDER BY col <-> literal LIMIT k` with no
        // WHERE and an NSW index on `col` skips the full scan. The
        // walk returns rows already in ascending-distance order, so
        // ORDER BY / LIMIT are honoured implicitly.
        if let Some(nsw_rows) = try_nsw_knn(stmt, table, schema_cols, alias) {
            return materialise_in_order(stmt, table, schema_cols, alias, &nsw_rows);
        }

        // Index seek: if WHERE is `col = literal` (or commuted) and the
        // referenced column has an index, dispatch each locator through
        // the catalog (hot tier → borrow, cold tier → page-read +
        // decode) and iterate just those rows. Otherwise fall back to a
        // full scan over the hot tier (cold-tier rows are only reached
        // via index seek in v5.1 — full table scans against cold-tier
        // data ship in v5.2 with the freezer's per-segment scan API).
        let indexed_rows: Option<Vec<Cow<'_, Row>>> = stmt.where_.as_ref().and_then(|w| {
            // BTree / col=literal seek first — covers the v7.11.3 multi-
            // column AND case and the leading-column equality lookup.
            try_index_seek(w, schema_cols, self.active_catalog(), table, alias)
                .or_else(|| {
                    // v7.12.3 — GIN-accelerated `WHERE col @@
                    // tsquery` when the column has a `USING gin`
                    // index. Returns an over-approximate candidate
                    // set; the WHERE re-eval loop below verifies
                    // the full `@@` predicate per row.
                    try_gin_seek(w, schema_cols, self.active_catalog(), table, alias, &ctx)
                })
                .or_else(|| {
                    // v7.15.0 — trigram-GIN-accelerated
                    // `WHERE col LIKE / ILIKE '<pat>'` when the
                    // column has a `gin_trgm_ops` GIN index.
                    // Over-approximate candidate set; the WHERE
                    // re-eval verifies the LIKE per row.
                    try_trgm_seek(w, schema_cols, table, alias)
                })
        });

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

    /// Constant `SELECT` with no FROM: evaluate each projection item
    /// once against an empty dummy row (`SELECT 1`, `SELECT '7'::INT`).
    fn exec_constant_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        let empty_schema: Vec<ColumnSchema> = Vec::new();
        let ctx = self.ev_ctx(&empty_schema, None);
        let projection = build_projection(&stmt.items, &empty_schema, "")?;
        let dummy_row = Row::new(Vec::new());
        let mut values = Vec::with_capacity(projection.len());
        for p in &projection {
            values.push(eval::eval_expr(&p.expr, &dummy_row, &ctx)?);
        }
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(QueryResult::Rows {
            columns,
            rows: alloc::vec![Row::new(values)],
        })
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
        indexed_rows: Option<Vec<Cow<'a, Row>>>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let mut filtered: Vec<&Row> = Vec::new();
        // v6.2.6 — Memoize: per-query LRU cache for correlated
        // scalar subqueries. Fresh per row-loop entry so each
        // SELECT execution gets an isolated cache.
        let mut memo = memoize::MemoizeCache::new();
        if let Some(rows) = &indexed_rows {
            for cow in rows {
                let row = cow.as_ref();
                if let Some(where_expr) = &stmt.where_ {
                    let cond = self.eval_expr_with_correlated(
                        where_expr,
                        row,
                        &ctx,
                        cancel,
                        Some(&mut memo),
                    )?;
                    if !matches!(cond, Value::Bool(true)) {
                        continue;
                    }
                }
                filtered.push(row);
            }
        } else {
            for i in 0..table.row_count() {
                let row = &table.rows()[i];
                if let Some(where_expr) = &stmt.where_ {
                    let cond = self.eval_expr_with_correlated(
                        where_expr,
                        row,
                        &ctx,
                        cancel,
                        Some(&mut memo),
                    )?;
                    if !matches!(cond, Value::Bool(true)) {
                        continue;
                    }
                }
                filtered.push(row);
            }
        }
        // v7.29 — a per-query memo so correlated scalar
        // subqueries batch-evaluate once (group map) instead of
        // executing per group.
        let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
        let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
            self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                .map_err(|err| match err {
                    EngineError::Eval(ev) => ev,
                    other => eval::EvalError::TypeMismatch {
                        detail: alloc::format!("{other}"),
                    },
                })
        };
        let filtered_rr: alloc::vec::Vec<RowRef<'_>> =
            filtered.iter().map(|&r| RowRef::Owned(r)).collect();
        let agg = aggregate::run(
            stmt,
            &filtered_rr,
            schema_cols,
            Some(alias),
            Some(&agg_correlated),
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
        indexed_rows: Option<Vec<Cow<'a, Row>>>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let projection = build_projection(&stmt.items, schema_cols, alias)?;
        // v7.19 P5 — single-table SELECT path for SRF
        // `SELECT unnest(arr) FROM t` shape. Detect a top-level
        // unnest in the projection list. When present, the
        // per-row processor emits one output row per array
        // element (broadcasting non-SRF projections from the
        // same input row). Empty / NULL arrays emit zero rows
        // for that input — PG semantics.
        let srf_position = projection.iter().position(|p| is_top_level_unnest(&p.expr));

        // Materialise the filter pass into `(order_key, projected_row)`
        // tuples. The order key is `None` when there's no ORDER BY clause.
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::new();
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
        let mut eval_stack: Vec<Value> = Vec::new();
        // Inline the per-row work in a closure so the indexed and full-
        // scan branches share the body.
        let mut process_row = |row: &Row, loop_idx: usize| -> Result<(), EngineError> {
            if loop_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(cw) = &compiled_where {
                let cond = eval::eval_compiled(cw, row, &ctx, &mut eval_stack)
                    .map_err(EngineError::Eval)?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            } else if let Some(where_expr) = &stmt.where_ {
                let cond =
                    self.eval_expr_with_correlated(where_expr, row, &ctx, cancel, Some(&mut memo))?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                build_order_keys(&stmt.order_by, row, &ctx)?
            };
            if let Some(srf_idx) = srf_position {
                let srf_arg = top_level_unnest_arg(&projection[srf_idx].expr)
                    .expect("checked by is_top_level_unnest above");
                let arr_val = eval::eval_expr(srf_arg, row, &ctx)?;
                let elements = array_value_to_elements(&arr_val)?;
                for elem in elements {
                    let mut values = Vec::with_capacity(projection.len());
                    for (i, p) in projection.iter().enumerate() {
                        if i == srf_idx {
                            values.push(elem.clone());
                        } else {
                            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
                        }
                    }
                    tagged.push((order_keys.clone(), Row::new(values)));
                }
            } else {
                let mut values = Vec::with_capacity(projection.len());
                for p in &projection {
                    // v7.24 (round-16 B) — correlated-aware.
                    values.push(self.eval_expr_with_correlated(&p.expr, row, &ctx, cancel, None)?);
                }
                tagged.push((order_keys, Row::new(values)));
            }
            Ok(())
        };
        if let Some(rows) = &indexed_rows {
            for (loop_idx, cow) in rows.iter().enumerate() {
                process_row(cow.as_ref(), loop_idx)?;
            }
        } else {
            for i in 0..table.row_count() {
                process_row(&table.rows()[i], i)?;
            }
        }

        if !stmt.order_by.is_empty() {
            // Partial-sort fast path: when LIMIT is small relative to
            // the row count, select_nth_unstable + sort just the
            // prefix is O(n + k log k) instead of O(n log n). DISTINCT
            // requires the full sort because de-dup happens after.
            // WITH TIES likewise needs the full sort so the tie
            // extension can scan past `limit` to find rows that
            // share the last-kept row's key.
            let keep = if stmt.distinct || stmt.limit_with_ties {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
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
        let output_rows: Vec<Row> = if stmt.limit_with_ties && !stmt.distinct {
            apply_offset_and_limit_tagged(
                &mut tagged,
                stmt.offset_literal(),
                stmt.limit_literal(),
                true,
            );
            tagged.into_iter().map(|(_, r)| r).collect()
        } else {
            let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
            if stmt.distinct {
                output_rows = dedup_rows(output_rows);
            }
            apply_offset_and_limit(
                &mut output_rows,
                stmt.offset_literal(),
                stmt.limit_literal(),
            );
            output_rows
        };

        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
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

    fn exec_joined_select(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
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
        let ctx = EvalContext::new(combined_schema, None);
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
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let agg = aggregate::run(stmt, &refs, combined_schema, None, Some(&agg_correlated))?;
            return self.finish_agg_result(agg, stmt, cancel);
        }

        let projection = build_projection(&stmt.items, combined_schema, "")?;
        // v7.32 (P4 increment 2) — projection / ORDER / DISTINCT / LIMIT
        // need owned Rows; materialise the survivors here (byte-identical
        // to the pre-deferral output).
        let filtered = deferred.materialise();
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::new();
        let mut proj_memo = memoize::MemoizeCache::default();
        for row in &filtered {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                // v7.24 (round-16 B) — select-list subqueries under a
                // JOIN go through the correlated-aware evaluator too.
                values.push(self.eval_expr_with_correlated(
                    &p.expr,
                    row,
                    &ctx,
                    cancel,
                    Some(&mut proj_memo),
                )?);
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                build_order_keys(&stmt.order_by, row, &ctx)?
            };
            let out_row = Row::new(values);
            budget.charge(approx_row_bytes(&out_row))?;
            tagged.push((order_keys, out_row));
        }
        if !stmt.order_by.is_empty() {
            let keep = if stmt.distinct {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }
        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(
            &mut output_rows,
            stmt.offset_literal(),
            stmt.limit_literal(),
        );
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
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
        let ctx = EvalContext::new(schema_cols, Some(alias));
        let seg = self
            .active_catalog()
            .cold_segment(segment_id)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "AS OF SEGMENT: cold segment {segment_id} not registered"
                ))
            })?;
        let mut out_rows: Vec<Row> = Vec::new();
        let mut limit_remaining: Option<usize> =
            stmt.limit_literal().and_then(|n| usize::try_from(n).ok());
        for (_key, body) in seg.scan() {
            let (row, _consumed) =
                spg_storage::decode_row_body_dense(&body, &schema, seg.codec_version())
                    .map_err(EngineError::Storage)?;
            if let Some(where_expr) = &stmt.where_ {
                let cond = self.eval_expr_simple(where_expr, &row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
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
        row: &Row,
        ctx: &EvalContext,
    ) -> Result<Value, EngineError> {
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
}

/// Dedupe a row set, preserving first-seen order. `Row`'s `PartialEq` is
/// structural (`Vec<Value>` ⇒ pairwise `Value` equality), which gives SQL
/// `NULL = NULL → TRUE` and `NaN = NaN → FALSE`. The first agrees with
/// the spec's "two NULLs are not distinct"; the second is a tolerated
/// quirk for v1 (no NaN literals are reachable from the SQL surface).
fn dedup_rows(rows: Vec<Row>) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for r in rows {
        if !out.iter().any(|seen| seen == &r) {
            out.push(r);
        }
    }
    out
}

/// Coerce a `Value` to an `f64` sort key for ORDER BY. Numbers map directly;
/// NULL sorts last (treated as `+∞`); booleans are 0.0 / 1.0; text uses lex
/// order via the byte values; vectors are not sortable.
pub(crate) fn value_to_order_key(v: &Value) -> Result<f64, EngineError> {
    match v {
        Value::Null => Ok(f64::INFINITY),
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        Value::Date(d) => Ok(f64::from(*d)),
        #[allow(clippy::cast_precision_loss)]
        Value::Timestamp(t) => Ok(*t as f64),
        // v7.17.0 Phase 3.P0-32 — PG TIME ordered by underlying
        // i64 microseconds (matches wall-clock ordering).
        #[allow(clippy::cast_precision_loss)]
        Value::Time(us) => Ok(*us as f64),
        // v7.17.0 Phase 3.P0-33 — MySQL YEAR ordered by underlying
        // u16 (matches calendar ordering; zero-year sentinel
        // sorts before 1901).
        Value::Year(y) => Ok(f64::from(*y)),
        // v7.17.0 Phase 3.P0-34 — PG TIMETZ ordered by the
        // UTC-equivalent microseconds (local wall - offset). Two
        // values for the same physical instant in different zones
        // sort equal — matches PG TIMETZ index behaviour.
        #[allow(clippy::cast_precision_loss)]
        Value::TimeTz { us, offset_secs } => Ok((us - i64::from(*offset_secs) * 1_000_000) as f64),
        // v7.17.0 Phase 3.P0-35 — PG MONEY ordered by i64 cents.
        #[allow(clippy::cast_precision_loss)]
        Value::Money(c) => Ok(*c as f64),
        // v7.17.0 Phase 3.P0-38 — range ordering is not supported
        // in v7.17.0 (needs lex-then-inclusivity tiebreak).
        Value::Range { .. } => Err(EngineError::Unsupported(
            "ORDER BY of a range value is not supported in v7.17.0".into(),
        )),
        // v7.17.0 Phase 3.P0-39 — hstore is not orderable.
        Value::Hstore(_) => Err(EngineError::Unsupported(
            "ORDER BY of a hstore value is not supported".into(),
        )),
        // v7.17.0 Phase 3.P0-40 — 2D arrays not orderable.
        Value::IntArray2D(_) | Value::BigIntArray2D(_) | Value::TextArray2D(_) => Err(
            EngineError::Unsupported("ORDER BY of a 2D array is not supported in v7.17.0".into()),
        ),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            // Scaled integer / 10^scale, computed via f64 for sort
            // ordering only. Precision losses here only matter for
            // ORDER BY tie-breaks well past 15 significant digits.
            // `f64::powi` lives in std; we hand-roll the loop so the
            // no_std engine crate doesn't need it.
            let mut divisor = 1.0_f64;
            for _ in 0..*scale {
                divisor *= 10.0;
            }
            Ok((*scaled as f64) / divisor)
        }
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Text(s) => {
            // Lex order by codepoints — good enough for ORDER BY name.
            // Map first 8 bytes packed into u64 as a coarse key; ties fall to
            // partial_cmp Equal. v1.x can swap in a real string comparator.
            let mut key: u64 = 0;
            for &b in s.as_bytes().iter().take(8) {
                key = (key << 8) | u64::from(b);
            }
            #[allow(clippy::cast_precision_loss)]
            Ok(key as f64)
        }
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            Err(EngineError::Unsupported(
                "ORDER BY of a raw vector column is not meaningful — use `<->`".into(),
            ))
        }
        Value::Interval { .. } => Err(EngineError::Unsupported(
            "ORDER BY of an INTERVAL is not supported in v2.11 \
             (months vs micros has no single canonical ordering)"
                .into(),
        )),
        Value::Json(_) => Err(EngineError::Unsupported(
            "ORDER BY of a JSON value is not supported — cast the document to text first".into(),
        )),
        // v7.5.0 — Value is #[non_exhaustive]; future variants need
        // an explicit ORDER BY mapping. Surface as Unsupported until
        // engine support is added.
        _ => Err(EngineError::Unsupported(
            "ORDER BY of this value type is not supported".into(),
        )),
    }
}

/// Find the schema entry that a SELECT-list `Expr::Column` refers to.
/// Mirrors `resolve_column` in `eval.rs`, but returns a proper
/// `EngineError` so the projection-build path keeps `UnknownQualifier`
/// vs `ColumnNotFound` distinct.
pub(crate) fn resolve_projection_column<'a>(
    c: &ColumnName,
    schema_cols: &'a [ColumnSchema],
    table_alias: &str,
) -> Result<&'a ColumnSchema, EngineError> {
    if let Some(q) = &c.qualifier {
        let composite = alloc::format!("{q}.{name}", name = c.name);
        if let Some(s) = schema_cols.iter().find(|s| s.name == composite) {
            return Ok(s);
        }
        // Single-table case: the qualifier may equal the active alias —
        // then look for the bare column name.
        if q == table_alias
            && let Some(s) = schema_cols.iter().find(|s| s.name == c.name)
        {
            return Ok(s);
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
        return Ok(s);
    }
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = schema_cols.iter().filter(|s| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Ok(s),
        (Some(_), Some(_)) => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!("ambiguous column reference: {}", c.name),
        })),
        _ => Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        })),
    }
}

pub(crate) fn build_projection(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Result<Vec<ProjectedItem>, EngineError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for col in schema_cols {
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name: col.name.clone(),
                        ty: col.ty,
                        nullable: col.nullable,
                    });
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
                    });
                } else if let Some(shape) = describe::describe_expr(expr, schema_cols) {
                    let output_name = alias.clone().unwrap_or_else(|| expr.to_string());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: shape.ty,
                        nullable: shape.nullable,
                    });
                } else {
                    let output_name = alias.clone().unwrap_or_else(|| expr.to_string());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: DataType::Text,
                        nullable: true,
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
pub(crate) fn infer_column_types(columns: &[ColumnSchema], rows: &[Row]) -> Vec<ColumnSchema> {
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

/// v4.22: encode a Row to a comparable byte key for UNION-DISTINCT
/// dedup inside the recursive iteration. Crude but deterministic
/// — Debug prints embed type discriminants so NULL ≠ "" ≠ 0.
fn encode_row_key(row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for v in &row.values {
        let s = alloc::format!("{v:?}|");
        out.extend_from_slice(s.as_bytes());
    }
    out
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
    cancel: &CancelToken<'_>,
) -> Result<alloc::vec::Vec<Row>, EngineError> {
    if step == 0 {
        return Err(EngineError::Unsupported(
            "generate_series(): step argument cannot be zero".into(),
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
        out.push(Row::new(alloc::vec![Value::BigInt(cur)]));
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
) -> Result<alloc::vec::Vec<Row>, EngineError> {
    let (months, micros) = match &step {
        Value::Interval { months, micros } => (*months, *micros),
        _ => unreachable!("caller guards step.is_interval"),
    };
    if months == 0 && micros == 0 {
        return Err(EngineError::Unsupported(
            "generate_series(): INTERVAL step cannot be zero".into(),
        ));
    }
    let ascending = months > 0 || micros > 0;
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
            &Value::Interval { months, micros },
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
            "FETCH FIRST … ROWS WITH TIES requires an ORDER BY clause",
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
    match expr {
        spg_sql::ast::Expr::FunctionCall { name, args } => {
            name.eq_ignore_ascii_case("unnest") && args.len() == 1
        }
        _ => false,
    }
}

/// v7.19 P5 — extract the array argument out of a top-level
/// `unnest(arg)` call. `None` if `expr` isn't a `unnest` call
/// of arity 1 (mirrors `is_top_level_unnest`).
fn top_level_unnest_arg(expr: &spg_sql::ast::Expr) -> Option<&spg_sql::ast::Expr> {
    match expr {
        spg_sql::ast::Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("unnest") && args.len() == 1 =>
        {
            Some(&args[0])
        }
        _ => None,
    }
}

/// v7.19 P5 — turn an array-typed `Value` into the element list
/// `unnest()` projection emits. NULL → empty list (PG: `unnest(NULL)
/// = (no rows)`). Non-array values fall through to a type-mismatch
/// error.
fn array_value_to_elements(v: &Value) -> Result<Vec<Value>, EngineError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::TextArray(items) => Ok(items
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|s| Value::Text(s.clone()))
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
        other => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!(
                "unnest() expects an array argument, got {:?}",
                other.data_type()
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
            let view = cat.views().get(name).ok_or_else(|| {
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
                body,
                recursive: false,
                column_overrides: view.columns.clone(),
            });
        }
        let mut out = stmt.clone();
        // Prepend so view CTEs are visible to caller-supplied CTEs.
        new_ctes.extend(out.ctes);
        out.ctes = new_ctes;
        Ok(Some(out))
    }
}
