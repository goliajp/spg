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
    try_gin_jsonb_seek, try_gin_seek, try_index_seek, try_nsw_knn, try_pk_walk_top_n,
    try_trgm_seek, value_is_integer, value_to_i64,
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
        let filtered: Vec<Row<'static>>;
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
            let mut owned: Vec<Row<'static>> = Vec::new();
            let mut emit = |row: &Row<'static>, i: usize| -> Result<(), EngineError> {
                if i.is_multiple_of(256) {
                    cancel.check()?;
                }
                if let Some(w) = &stmt.where_ {
                    let cond = eval::eval_expr(w, row, &ctx)?;
                    if !matches!(cond, Value::Bool(true)) {
                        return Ok(());
                    }
                }
                owned.push(row.clone());
                Ok(())
            };
            // v7.37.15 Phase B — scan_visible filters rows by the
            // engine's current snapshot. Phase B's `current_snapshot()`
            // returns `Snapshot::unbounded()` so every row is visible,
            // matching pre-v7.37.15 byte-for-byte. Phase C will wire
            // real per-tx snapshots through this same callsite — no
            // code change needed here when that lands.
            let snap = self.current_snapshot();
            for (i, row) in table.scan_visible(&snap) {
                emit(row, i)?;
            }
            // v7.36 (cold-tier coverage) — window single-table path
            // mirrors `run_single_table_scan`: hot iter then cold iter,
            // both routed through the same `emit` so WHERE / clone /
            // cancel-poll semantics stay byte-identical.
            let hot_len = table.row_count();
            for (offset, row) in self.iter_cold_rows_of_table(table).iter().enumerate() {
                emit(row, hot_len + offset)?;
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
        // `compute_window_partition` call (which takes `&[&Row<'static>]`) and
        // the per-row eval loops share a single backing buffer.
        let filtered_refs: Vec<&Row<'static>> = filtered.iter().collect();

        // 2) Collect unique window function nodes from projection.
        let mut window_nodes: Vec<Expr> = Vec::new();
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_window_nodes(expr, &mut window_nodes);
            }
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
            } = wnode
            else {
                unreachable!("collect_window_nodes pushes only WindowFunction");
            };
            // Compute (partition_key, order_key, original_index) for each row.
            let mut indexed: Vec<(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)> =
                Vec::with_capacity(n_rows);
            for (i, row) in filtered.iter().enumerate() {
                let pkey: Vec<Value<'static>> = partition_by
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
        let mut ext_rows: Vec<Row<'static>> = Vec::with_capacity(n_rows);
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
        let mut out_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
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
                // v7.37.24 (24.1) — pg_catalog.pg_enum (label list
                // for ENUM types; sqlx / ORM enum codecs read this).
                "__spg_pg_enum" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_enum(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13) — pg_catalog.pg_replication_slots
                // (shape-stable empty until 21.12 persists slot state).
                "__spg_pg_replication_slots" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_replication_slots(
                            self.active_catalog(),
                        );
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
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_database(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.14) — pg_catalog.pg_stat_user_tables
                // (per-table churn counters; live_tup = row count).
                "__spg_pg_stat_user_tables" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_user_tables(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.15) — pg_catalog.pg_stat_user_indexes
                // (per-index usage counters; flag unused indexes).
                "__spg_pg_stat_user_indexes" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_user_indexes(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.16) — pg_catalog.pg_stat_bgwriter.
                "__spg_pg_stat_bgwriter" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_bgwriter(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.22 (22.17) — pg_catalog.pg_stat_archiver.
                "__spg_pg_stat_archiver" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_archiver(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.21 (21.13-d) — pg_catalog.pg_stat_replication.
                "__spg_pg_stat_replication" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_stat_replication(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.13) — pg_catalog.pg_am.
                "__spg_pg_am" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_am(self.active_catalog());
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
                        crate::system_catalog::synth_pg_stat_user_functions(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.23 (23.7-a) — pg_catalog.pg_statistic_ext.
                "__spg_pg_statistic_ext" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_statistic_ext(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.15) — pg_catalog.pg_statistic.
                "__spg_pg_statistic" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_pg_statistic(self.active_catalog());
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
                // v7.37.24 (24.3) — information_schema.attributes.
                "__spg_info_attributes" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_information_schema_attributes(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.2) — information_schema.domains.
                "__spg_info_domains" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_information_schema_domains(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.9) — information_schema.schemata.
                "__spg_info_schemata" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_information_schema_schemata(
                            self.active_catalog(),
                        );
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.37.24 (24.9) — information_schema.views.
                "__spg_info_views" => {
                    let (schema, rows) =
                        crate::system_catalog::synth_information_schema_views(
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
        // v7.37.43-T4.4 — `&self` SELECT path: only read-only CTE
        // bodies are supported here. Writable CTEs on a SELECT
        // outer require `&mut self` and route through the
        // top-level `exec_select_cancel_mut` entry; sentori
        // 0065's WITH-INSERT-INSERT shape comes in as a top-level
        // INSERT, not a SELECT, so this restriction is harmless
        // in practice.
        if stmt.ctes.iter().any(|c| c.body.is_modifying()) {
            return Err(EngineError::Unsupported(alloc::format!(
                "SELECT with a data-modifying CTE body must run via the top-level mutable entry"
            )));
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
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let body_select = cte.body.as_select().ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "data-modifying CTE not supported on this SELECT entry"
                ))
            })?;
            let (columns, rows) = if cte.recursive {
                let synthetic = spg_sql::ast::Cte {
                    name: cte.name.clone(),
                    body: spg_sql::ast::CteBody::Select(body_select.clone()),
                    recursive: true,
                    column_overrides: cte.column_overrides.clone(),
                };
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
        for cte in ctes {
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let (columns, rows) = match &cte.body {
                spg_sql::ast::CteBody::Select(body) if cte.recursive => {
                    // Recursive CTE — the existing helper takes a
                    // SELECT body and the snapshot catalog.
                    let synthetic = spg_sql::ast::Cte {
                        name: cte.name.clone(),
                        body: spg_sql::ast::CteBody::Select(body.clone()),
                        recursive: true,
                        column_overrides: cte.column_overrides.clone(),
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
        // v7.37.43-T4.4 — strip any nested CTEs from the body
        // (already materialised in the outer pass) before
        // dispatch to avoid infinite recursion.
        let mut body = body.clone();
        body.ctes = Vec::new();
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
        let mut body = body.clone();
        body.ctes = Vec::new();
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
        let mut body = body.clone();
        body.ctes = Vec::new();
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
            let mut next_set: Vec<Row<'static>> = Vec::new();
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
        row: &Row<'static>,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        alias: &str,
    ) -> Result<Row<'static>, EngineError> {
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
        // v7.38 P0 元机制 A — first observable point inside the
        // planner / executor. Tests use this to inject a delay or
        // a cancellation race before any row is produced. Release
        // build expands to `let _ = (...);` — zero cost.
        crate::injection_point!("planner_first_row_fetch", &stmt.from);
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
                // v7.37.7 — PG's `pg_stat_statements` extension view.
                // v7.37.22 (22.1) — full PG-shape view (38 columns)
                // backed by the same query_stats registry that powers
                // spg_stat_query. Tools that query specific PG columns
                // (`SELECT total_exec_time FROM pg_stat_statements
                // ORDER BY total_exec_time DESC`) now resolve those
                // columns directly. `spg_stat_query` keeps its
                // simplified shape for the human-facing spgctl path.
                "pg_stat_statements" => return Ok(self.exec_pg_stat_statements()),
                "spg_stat_activity" => return Ok(self.exec_spg_stat_activity()),
                // v7.37.14 (B6.5) — PG-compatibility surface; row
                // set is empty until v7.37.15 lands tuple locks.
                "pg_locks" => return Ok(self.exec_pg_locks()),
                // v7.37.22 (22.2) — per-relation I/O counters.
                "pg_statio_user_tables" => {
                    return Ok(self.exec_pg_statio_user_tables());
                }
                // v7.37.15 (Phase F) — MVCC diagnostic view; per-
                // process snapshot of the writer-version cursor +
                // in-flight tx versions. Used by spgctl / dashboards
                // to observe MVCC health (vacuum lag, in-flight
                // tx count).
                "spg_stat_mvcc" => return Ok(self.exec_spg_stat_mvcc()),
                // v7.37.16 (16.11) — per-partition health row set.
                "spg_partition_health" => return Ok(self.exec_spg_partition_health()),
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
        let (elem_dtype, rows): (DataType, alloc::vec::Vec<Row<'static>>) =
            match eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)? {
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
        let mut arg_values: alloc::vec::Vec<Value<'static>> =
            alloc::vec::Vec::with_capacity(args.len());
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
        let mut projected_rows: alloc::vec::Vec<Row<'static>> =
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
                return self.exec_bare_select_cancel(&eliminated, cancel);
            }
            // v7.38 P0 元机制 D — `SPG_TEST_DISABLE_JOINFOLD=1` skips
            // the v7.32 joinfold rewrite that turns inner JOINs into a
            // single-table scan when the catalogue can prove key-only
            // dependency. Tests use this to assert "without joinfold,
            // the join still executes correctly" (joinfold is a
            // semantically-equivalent rewrite, not a correctness fix).
            if !self.env_cfg().disable_joinfold {
                if let Some(folded) = self.try_fold_inner_joins(stmt, cancel)? {
                    return self.exec_bare_select_cancel(&folded, cancel);
                }
            }
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
        // v7.37.43-T4.5 — `FROM jsonb_each_text(<expr>)` set-
        // returning function. Same dispatch shape as unnest but
        // emits a two-column (key TEXT, value TEXT) row stream.
        if from.primary.jsonb_each_text_arg.is_some() {
            return self.exec_select_jsonb_each_text(stmt, &from.primary, cancel);
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
            // NSW kNN dispatches against the hot-tier vector index only
            // (vector cells aren't promoted to cold segments), so wrap
            // the returned row indices as `Cow::Borrowed` for the
            // unified `materialise_in_order` shape.
            let ordered: Vec<Cow<'_, Row<'static>>> = nsw_rows
                .into_iter()
                .filter_map(|i| table.rows().get(i).map(Cow::Borrowed))
                .collect();
            return materialise_in_order(stmt, schema_cols, alias, &ordered);
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
            return materialise_in_order(stmt, schema_cols, alias, &walked);
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
            return Ok(out);
        }
        // full scan over the hot tier (cold-tier rows are only reached
        // via index seek in v5.1 — full table scans against cold-tier
        // data ship in v5.2 with the freezer's per-segment scan API).
        let indexed_rows: Option<Vec<Cow<'_, Row<'static>>>> = stmt.where_.as_ref().and_then(|w| {
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
                .or_else(|| {
                    // v7.37.8(sentori Epic 5 P2)— real JSONB-GIN
                    // accelerated `WHERE col @> <jsonb_literal>`
                    // when the column has a `USING gin` index. The
                    // posting-list intersection returns an over-
                    // approximate candidate set; the WHERE re-eval
                    // verifies the full `@>` predicate per row.
                    try_gin_jsonb_seek(w, schema_cols, table, alias)
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

    /// v7.37.43-T4.5 — execute `SELECT … FROM jsonb_each_text(<expr>)`.
    /// Sentori migration 0067 uses this with `CROSS JOIN LATERAL`; the
    /// uncorrelated FROM-primary case is the simpler shape, used by
    /// e2e pins. Materialises the (key, value) pair stream into a
    /// synthetic two-column TEXT table, then routes through the
    /// regular projection / WHERE / ORDER BY pipeline.
    fn exec_select_jsonb_each_text(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let arg_expr = primary
            .jsonb_each_text_arg
            .as_deref()
            .expect("caller guards jsonb_each_text_arg.is_some()");
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        let arg_value = eval::eval_expr(arg_expr, &dummy_row, &ctx).map_err(EngineError::Eval)?;
        let pairs = crate::json::jsonb_each_text_rows(&arg_value).map_err(EngineError::Eval)?;
        let rows: alloc::vec::Vec<Row<'static>> = pairs
            .into_iter()
            .map(|(k, v)| {
                let key_val = Value::text(k);
                let value_val = match v {
                    Some(s) => Value::text(s),
                    None => Value::Null,
                };
                Row::new(alloc::vec![key_val, value_val])
            })
            .collect();
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "jsonb_each_text".to_string());
        let key_col = ColumnSchema::new("key".to_string(), spg_storage::DataType::Text, false);
        let value_col = ColumnSchema::new("value".to_string(), spg_storage::DataType::Text, true);
        let schema_cols = alloc::vec![key_col, value_col];
        let scan_ctx = EvalContext::new(&schema_cols, Some(&alias));
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
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
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
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let mut filtered: Vec<&Row<'static>> = Vec::new();
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
                    let cond = eval::eval_compiled(cw, row, &ctx, eval_stack)
                        .map_err(EngineError::Eval)?;
                    Ok(matches!(cond, Value::Bool(true)))
                }
                (None, Some(w)) => {
                    let cond = self.eval_expr_with_correlated(w, row, &ctx, cancel, Some(memo))?;
                    Ok(matches!(cond, Value::Bool(true)))
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
            for i in 0..table.row_count() {
                let row = &table.rows()[i];
                if !row_passes_where(row, &mut eval_stack, &mut memo)? {
                    continue;
                }
                filtered.push(row);
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
        indexed_rows: Option<Vec<Cow<'a, Row<'static>>>>,
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
        let mut tagged: Vec<(Vec<f64>, Row<'static>)> = Vec::new();
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
        // v7.37.x (docker-fair SCALARSQ attack) — early-limit gate for
        // the no-ORDER-BY-no-DISTINCT-no-TIES-no-SRF-no-WHERE shape.
        // Hoisted above the closure so the projection-eval path can
        // gate `memo` passing on it: the SELECT-item correlated-scalar
        // batch path scans the FULL inner table once (~5 ms for 12.5 k
        // rows) and is only a win when N outer rows is large; for small
        // LIMITed shapes a per-row PK seek (~5 µs × 100 = 500 µs) wins.
        let early_cap: Option<usize> = if stmt.order_by.is_empty()
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
        // Inline the per-row work in a closure so the indexed and full-
        // scan branches share the body.
        let mut process_row = |row: &Row<'static>, loop_idx: usize| -> Result<(), EngineError> {
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
                    let mut values: Vec<Value<'static>> = Vec::with_capacity(projection.len());
                    for (i, p) in projection.iter().enumerate() {
                        if i == srf_idx {
                            values.push(elem.clone());
                        } else {
                            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
                        }
                    }
                    let out = Row::new(values);
                    budget.charge(approx_row_bytes(&out))?;
                    tagged.push((order_keys.clone(), out));
                }
            } else {
                let mut values: Vec<Value<'static>> = Vec::with_capacity(projection.len());
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
                let out = Row::new(values);
                budget.charge(approx_row_bytes(&out))?;
                tagged.push((order_keys, out));
            }
            Ok(())
        };
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
            for i in 0..table.row_count() {
                if let Some(cap) = early_cap
                    && emitted >= cap
                {
                    break;
                }
                process_row(&table.rows()[i], i)?;
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

        if !stmt.order_by.is_empty() {
            // Partial-sort fast path: when LIMIT is small relative to
            // the row count, select_nth_unstable + sort just the
            // prefix is O(n + k log k) instead of O(n log n). DISTINCT
            // requires the full sort because de-dup happens after.
            // WITH TIES likewise needs the full sort so the tie
            // extension can scan past `limit` to find rows that
            // share the last-kept row's key.
            let keep = if stmt.distinct
                || stmt.limit_with_ties
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
        let output_rows: Vec<Row<'static>> = if stmt.limit_with_ties && !stmt.distinct {
            apply_offset_and_limit_tagged(
                &mut tagged,
                stmt.offset_literal(),
                stmt.limit_literal(),
                true,
            );
            tagged.into_iter().map(|(_, r)| r).collect()
        } else {
            let mut output_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
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
        if from.joins.is_empty() {
            return Ok(None);
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
        let ctx = EvalContext::new(combined_schema, None);
        let projection = build_projection(&stmt.items, combined_schema, "")?;
        // Every projection item must be a bound qualified column —
        // anything that needs `eval_expr_with_correlated` keeps the
        // materialising path.
        let bound_pos = |e: &Expr| -> Option<usize> {
            match e {
                Expr::Column(c) if c.qualifier.is_some() => eval::find_column_pos(c, &ctx),
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
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
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
            emit(crate::StreamItem::Row(&cell_refs))?;
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
            let agg_correlated = |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| {
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
        // ORDER BY (when present) still evaluates against a materialised
        // Row — keep the order-key encoder correct rather than fork it.
        let need_eval_row = !all_proj_bound || !stmt.order_by.is_empty();
        let mut tagged: Vec<(Vec<f64>, Row<'static>)> = Vec::new();
        let mut proj_memo = memoize::MemoizeCache::default();
        let sources_ref = &deferred.sources;
        let stride = deferred.stride;
        let survivors_ref = &deferred.survivors;
        let n_surv = survivors_ref.len() / stride.max(1);
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
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                let mrow = materialised.as_deref().expect("materialised for order by");
                build_order_keys(&stmt.order_by, mrow, &ctx)?
            };
            let out_row = Row::new(values);
            budget.charge(approx_row_bytes(&out_row))?;
            tagged.push((order_keys, out_row));
        }
        if !stmt.order_by.is_empty() {
            let keep = if stmt.distinct
                // v7.38 元机制 D acceptor — see other call site above.
                || self.env_cfg().disable_topk
            {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }
        let mut output_rows: Vec<Row<'static>> = tagged.into_iter().map(|(_, r)| r).collect();
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
        let mut out_rows: Vec<Row<'static>> = Vec::new();
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
}

/// Dedupe a row set, preserving first-seen order. `Row`'s `PartialEq` is
/// structural (`Vec<Value<'static>>` ⇒ pairwise `Value` equality), which gives SQL
/// `NULL = NULL → TRUE` and `NaN = NaN → FALSE`. The first agrees with
/// the spec's "two NULLs are not distinct"; the second is a tolerated
/// quirk for v1 (no NaN literals are reachable from the SQL surface).
fn dedup_rows(rows: Vec<Row<'static>>) -> Vec<Row<'static>> {
    let mut out: Vec<Row<'static>> = Vec::with_capacity(rows.len());
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
fn encode_row_key(row: &Row<'static>) -> Vec<u8> {
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
) -> Result<alloc::vec::Vec<Row<'static>>, EngineError> {
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
fn array_value_to_elements(v: &Value) -> Result<Vec<Value<'static>>, EngineError> {
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
                body: spg_sql::ast::CteBody::Select(body),
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
        let mut body = alloc::string::String::new();
        for (i, child_name) in kept.iter().enumerate() {
            if i > 0 {
                body.push_str(" UNION ALL ");
            }
            body.push_str("SELECT * FROM ");
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
    if !parents.iter().any(|p| p == &t.name) {
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
    if crate::partition::is_partition_parent(cat, &t.name) {
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
