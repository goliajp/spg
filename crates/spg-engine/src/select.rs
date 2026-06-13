//! SELECT execution — the window / meta-view / CTE variants and the
//! subquery-resolution pre-pass. Lifted out of `lib.rs` (v7.32 engine
//! modularisation). These `impl Engine` methods are dispatched from the
//! bare-SELECT entry points and drive the non-trivial SELECT shapes.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement, UnionKind};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, StorageError, TableSchema, Value};

use crate::eval::EvalContext;
use crate::{
    ByteBudget, CancelToken, Engine, EngineError, QueryResult, apply_offset_and_limit,
    build_projection, collect_meta_view_names, collect_window_nodes, compute_window_partition,
    encode_row_key, eval, infer_column_types, materialise_meta_view, order_key_cmp,
    partition_key_cmp, rewrite_window_to_columns, select_refers_to, sort_by_keys,
    synth_info_key_column_usage, synth_info_referential_constraints, synth_info_routines,
    synth_info_statistics, synth_information_schema_columns, synth_information_schema_tables,
    synth_mysql_db, synth_mysql_user, synth_pg_attribute, synth_pg_class, synth_pg_constraint,
    synth_pg_database, synth_pg_extension, synth_pg_index_raw, synth_pg_indexes,
    synth_pg_namespace, synth_pg_proc, synth_pg_roles, synth_pg_settings, synth_pg_trigger,
    synth_pg_type, synth_pg_views, value_to_order_key,
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
