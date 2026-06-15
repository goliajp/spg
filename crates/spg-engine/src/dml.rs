//! DML execution — the row-mutating statements (INSERT / UPDATE /
//! MERGE / DELETE) lifted out of `lib.rs` (v7.32 engine
//! modularisation). These `impl Engine` methods are dispatched from
//! `Engine::execute` and reach deep into the storage / catalog / WAL
//! internals, which `lib.rs` exposes pub(crate) for them.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, InsertStatement, SelectItem};
use spg_storage::{ColumnSchema, Row, StorageError, Value};

use crate::eval::{EvalContext, EvalError};
use crate::{
    CancelToken, Engine, EngineError, QueryResult, any_column_changed, apply_fk_child_step,
    apply_on_conflict_assignments, canonicalize_set_value, check_unsigned_range, coerce_value,
    enforce_check_constraints, enforce_enum_label, enforce_fk_inserts,
    enforce_unique_index_inserts, enforce_uniqueness_inserts, eval, eval_runtime_default_free,
    expr_has_subquery, literal_expr_to_value, lookup_row_position_by_keys, on_conflict_keys_exist,
    plan_fk_parent_deletions, plan_fk_parent_updates, resolve_column_default_free,
    resolve_on_conflict_columns, triggers, try_index_seek_positions, try_pk_predicate,
    value_to_literal_expr_permissive,
};

/// Pre-borrow snapshots gathered by `prepare_insert_snapshots` for the
/// INSERT row loop, taken before the mutable catalog borrow opens.
struct InsertSnapshots {
    clock: Option<crate::ClockFn>,
    before_insert_triggers: Vec<spg_storage::FunctionDef>,
    after_insert_triggers: Vec<spg_storage::FunctionDef>,
    trigger_session_cfg: Option<String>,
    enum_label_lookup: alloc::collections::BTreeMap<usize, Vec<String>>,
    set_variant_lookup: alloc::collections::BTreeMap<usize, Vec<String>>,
    seq_floors: alloc::collections::BTreeMap<usize, i64>,
}

impl Engine {
    /// v4.4 `UPDATE <table> SET col = expr [, ...] [WHERE cond]`.
    /// Filter pass uses the same WHERE eval as `exec_select`. Per
    /// matched row, evaluate each RHS expression against the *old*
    /// row, then call `Table::update_row` which rebuilds indices.
    /// Indexed columns are correctly reflected because rebuild
    /// happens after the cell rewrite.
    pub(crate) fn exec_update_cancel(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.12.5 — snapshot BEFORE/AFTER UPDATE row triggers + the
        // session FTS config before the table mut-borrow opens (the
        // INSERT path uses the same pattern). Empty vecs are the
        // common "no triggers on this table" fast path.
        // v7.13.0 — UPDATE triggers carry an optional `UPDATE OF
        // cols` filter. The filter is paired with each function so
        // the per-row fire loop can skip when no listed column
        // actually differs between OLD and NEW.
        let before_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "BEFORE");
        let after_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: if the WHERE is a PK equality and matches a cold-
        // tier row, promote it back to the hot tier *before* the
        // hot-row walk. The promote pushes the row to the end of
        // `table.rows`, where the upcoming SET-evaluation loop will
        // pick it up and apply the assignments. Lookups for the key
        // never observe a gap because `promote_cold_row` inserts the
        // hot row before retiring the cold locator.
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                // Promote may be a no-op (key is hot-only or absent);
                // we don't care about the return value here — the
                // subsequent hot walk will either match or not.
                let _ = self
                    .active_catalog_mut()
                    .promote_cold_row(&stmt.table, &idx_name, &key);
            }
        }

        // v7.12.1 — cache session FTS config before the table
        // mut-borrow (same reason as exec_delete).
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        // v7.17.0 Phase 2.1 — snapshot the clock pointer before
        // we hold the catalog mutably so ON UPDATE runtime
        // overrides see the engine wall clock.
        let clock_for_on_update = self.clock;
        // v7.31 (mailrs round-28) — the candidate-gathering phase
        // below is READ-ONLY (it builds `planned`; the mutation
        // happens after `let _ = table`). Borrow the catalog
        // SHARED here so a correlated scalar subquery in SET / WHERE
        // can re-enter the engine read path (`eval_expr_with_correlated`
        // also takes `&self`) with the target row as its outer
        // context. The apply phase re-acquires `active_catalog_mut()`.
        let table = self.active_catalog().get(&stmt.table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: stmt.table.clone(),
            })
        })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        // Resolve each SET target to a column position once, validate
        // up front so a typo'd column doesn't leave a partial mutation
        // behind.
        let mut targets: Vec<(usize, &Expr)> = Vec::with_capacity(stmt.assignments.len());
        for (col, expr) in &stmt.assignments {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == *col)
                .ok_or_else(|| {
                    EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                })?;
            targets.push((pos, expr));
        }
        // v7.17.0 Phase 2.1 — for every column with an
        // `ON UPDATE CURRENT_TIMESTAMP` binding that the caller
        // did NOT explicitly set, schedule an automatic override.
        // Reuses `eval_runtime_default_free` so the same
        // canonical runtime-expression whitelist (now /
        // current_timestamp / current_date / …) governs both
        // DEFAULT and ON UPDATE.
        let mut on_update_overrides: Vec<(usize, String)> = Vec::new();
        for (i, col) in schema_cols.iter().enumerate() {
            if targets.iter().any(|(p, _)| *p == i) {
                continue;
            }
            if let Some(src) = &col.on_update_runtime {
                on_update_overrides.push((i, src.clone()));
            }
        }
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()))
            .with_default_text_search_config(ts_cfg.as_deref());
        // Walk candidate rows, evaluate WHERE then SET
        // expressions. We gather (position, new_values) tuples
        // first and apply them afterwards so the WHERE/RHS
        // evaluation reads the original row state — matches PG
        // semantics (UPDATE doesn't see its own writes).
        //
        // v7.20 P4 — index seek: a single-column equality WHERE
        // on an indexed column narrows the walk from
        // O(table.rows()) to O(matches). The full WHERE still
        // re-evaluates per candidate (the seek may be an
        // over-approximation under AND-composites), so semantics
        // are unchanged. profile: the bench's `UPDATE … WHERE
        // id = $1` on a 5 000-row table was a ~1.3 ms full scan
        // per statement; with the seek it's ~2 µs.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let mut planned: Vec<(usize, Vec<Value>)> = Vec::new();
        let candidate_positions: Vec<usize> = match &seek_positions {
            Some(list) => list.clone(),
            None => (0..table.row_count()).collect(),
        };
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            // v4.5: cooperative cancel checkpoint every 256 rows so
            // a runaway UPDATE without WHERE doesn't drag past the
            // server's query-timeout watchdog.
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            if let Some(w) = &stmt.where_ {
                // v7.31 (round-28) — correlated subqueries in the
                // UPDATE WHERE bind to the candidate row, like the
                // SELECT path; plain predicates keep the cheap
                // interpreter.
                let cond = if expr_has_subquery(w) {
                    self.eval_expr_with_correlated(w, row, &ctx, cancel, None)?
                } else {
                    eval::eval_expr(w, row, &ctx)?
                };
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut new_vals = row.values.clone();
            for (pos, expr) in &targets {
                // v7.31 (round-28) — correlated scalar subquery in
                // SET (`SET c = (SELECT … WHERE x = target.col)`)
                // binds the target row as its outer context. The
                // non-correlated case was materialised up front by
                // resolve_expr_subqueries at the UPDATE entry.
                let v = if expr_has_subquery(expr) {
                    self.eval_expr_with_correlated(expr, row, &ctx, cancel, None)?
                } else {
                    eval::eval_expr(expr, row, &ctx)?
                };
                let coerced = coerce_value(v, schema_cols[*pos].ty, &schema_cols[*pos].name, *pos)?;
                check_unsigned_range(&coerced, &schema_cols[*pos], *pos)?;
                new_vals[*pos] = coerced;
            }
            // v7.17.0 Phase 2.1 — apply ON UPDATE overrides for
            // any column the SET clause didn't touch.
            for (pos, src) in &on_update_overrides {
                let v = eval_runtime_default_free(src, schema_cols[*pos].ty, clock_for_on_update)?;
                new_vals[*pos] = v;
            }
            planned.push((i, new_vals));
        }
        // planned must stay position-sorted: downstream passes
        // (FK pairing, trigger walks, the apply loop) iterate it
        // assuming ascending row order, which the full-scan path
        // guaranteed implicitly.
        planned.sort_by_key(|(i, _)| *i);
        // v7.6.6 — capture pre-update row values for the FK
        // enforcement passes below. `planned` carries new values
        // only; pair them with the old row.
        let plan_with_old: Vec<(usize, Vec<Value>, Vec<Value>)> = planned
            .iter()
            .map(|(pos, new_vals)| (*pos, table.rows()[*pos].values.clone(), new_vals.clone()))
            .collect();
        let self_fks = table.schema().foreign_keys.clone();
        // v7.12.5 — `affected` is computed post-BEFORE-trigger
        // below (triggers may RETURN NULL to skip individual
        // rows). The pre-trigger len shape is no longer accurate.
        // Release mutable borrow on `table` for the FK passes.
        let _ = table;
        // v7.6.6 — Stage 2a: outbound FK check. For every row whose
        // local FK columns changed, the new value must exist in the
        // parent.
        if !self_fks.is_empty() {
            let new_rows: Vec<Vec<Value>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            enforce_fk_inserts(self.active_catalog(), &stmt.table, &self_fks, &new_rows)?;
        }
        // v7.13.0 — CHECK constraint enforcement on UPDATE
        // (mailrs round-5 G3). Predicates evaluated against the
        // candidate post-UPDATE row; false rejects the UPDATE.
        {
            let new_rows: Vec<Vec<Value>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            enforce_check_constraints(self.active_catalog(), &stmt.table, &new_rows)?;
        }
        // v7.6.6 — Stage 2b: inbound FK check. For every row that
        // changed value in a column that *some other table* uses as
        // a FK parent column, react per `on_update` action.
        let child_plan =
            plan_fk_parent_updates(self.active_catalog(), &stmt.table, &plan_with_old)?;
        // Stage 3a — apply each child-side action.
        for step in &child_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // Stage 3b — apply the original UPDATE.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v7.12.5 — fire BEFORE/AFTER UPDATE row-level triggers
        // around the apply loop. BEFORE sees NEW=candidate +
        // OLD=current; may rewrite NEW or RETURN NULL to skip.
        // AFTER sees NEW=post-write + OLD=pre-write (both read-
        // only).
        //
        // Filter `planned` through the BEFORE pass first so the
        // RETURNING snapshot reflects what actually got written
        // (triggers may rewrite cells, including a cancellation).
        let mut applied_after_before: Vec<(usize, Row, Row)> = Vec::with_capacity(planned.len());
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        for (pos, new_vals) in &planned {
            let old_row = table.rows()[*pos].clone();
            let mut new_row = Row::new(new_vals.clone());
            let mut skip = false;
            for (fd, filter) in &before_update_triggers {
                // v7.13.0 — `UPDATE OF cols` filter (mailrs round-5
                // G7). Skip this trigger when the filter is set and
                // no listed column actually differs between OLD and
                // NEW for this row.
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    false,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => new_row = r,
                    triggers::TriggerOutcome::Skip => {
                        skip = true;
                        break;
                    }
                }
            }
            if !skip {
                applied_after_before.push((*pos, new_row, old_row));
            }
        }
        // v7.9.4 — snapshot post-update values for RETURNING (post-
        // BEFORE-trigger because triggers can rewrite cells).
        let updated_for_returning: Vec<Vec<Value>> = if stmt.returning.is_some() {
            applied_after_before
                .iter()
                .map(|(_pos, new_row, _old)| new_row.values.clone())
                .collect()
        } else {
            Vec::new()
        };
        let affected = applied_after_before.len();
        // Apply, then fire AFTER triggers per row. AFTER runs read-
        // only against the freshly-written row; v7.12.4-shape
        // assignment errors with a clear message.
        for (pos, new_row, old_row) in applied_after_before {
            table.update_row(pos, new_row.values.clone())?;
            for (fd, filter) in &after_update_triggers {
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                let (_outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    true,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
            }
        }
        let _ = table;
        // v7.12.7 — drain trigger-emitted embedded SQL for this UPDATE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for UPDATE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.9.4 — RETURNING projection.
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, updated_for_returning);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Collects matching
    /// positions then delegates to `Table::delete_rows` (single index
    /// rebuild for the batch).
    /// v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ `MERGE` execution.
    ///
    /// Semantics:
    ///   * Resolve `target` and `source` tables (catalog reads).
    ///   * Build a combined `(target_alias.col, source_alias.col)`
    ///     schema so the ON / WHEN AND / SET / VALUES expressions
    ///     resolve through the standard qualifier-aware resolver.
    ///   * Pass 1: walk every source row × every target hot row,
    ///     evaluate ON, then pick the first WHEN clause that fits
    ///     (`Matched` if any target row matched, `NotMatched`
    ///     otherwise; AND-condition must hold). Collect the action
    ///     plan as `(deletes, updates, inserts)` so the apply pass
    ///     reads the original target row state.
    ///   * Pass 2: apply the plan against the target's mutable row
    ///     vector. Deletes execute by index in descending order so
    ///     earlier indices remain stable; updates next; inserts
    ///     last (matching PG's "INSERT branch sees the post-delete
    ///     state" behaviour for the common upsert shape).
    ///
    /// v7.17 simplifications (documented limitations):
    ///   * No triggers / WAL plumbing (MVP); MERGE rows don't fire
    ///     INSERT / UPDATE / DELETE row triggers in v7.17.
    ///   * No cardinality check (PG-canonical: "MERGE command
    ///     cannot affect row a second time" — SPG silently applies
    ///     the last action for a target row covered twice).
    ///   * Source must be a catalog-resolvable table (no subquery
    ///     source); RETURNING / BY SOURCE / BY TARGET unsupported.
    pub(crate) fn exec_merge_cancel(
        &mut self,
        stmt: &spg_sql::ast::MergeStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let target_alias = stmt
            .target_alias
            .clone()
            .unwrap_or_else(|| stmt.target.clone());
        let source_alias = stmt
            .source_alias
            .clone()
            .unwrap_or_else(|| stmt.source.clone());
        let (target_cols, target_rows_snapshot) = {
            let t = self.active_catalog().get(&stmt.target).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
            (
                t.schema().columns.clone(),
                t.rows().iter().cloned().collect::<Vec<Row>>(),
            )
        };
        let (source_cols, source_rows) = {
            let s = self.active_catalog().get(&stmt.source).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.source.clone(),
                })
            })?;
            (
                s.schema().columns.clone(),
                s.rows().iter().cloned().collect::<Vec<Row>>(),
            )
        };
        // Composite schema: target_alias.col ... source_alias.col ...
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let combined_ctx = EvalContext::new(&combined_schema, None);
        // Source-only context for WHEN NOT MATCHED actions (no
        // matched target row exists — the source-side qualified
        // columns must still resolve).
        let mut source_only_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let source_only_ctx = EvalContext::new(&source_only_schema, None);
        let target_arity = target_cols.len();
        let source_arity = source_cols.len();

        // Resolve INSERT column positions once (validate names).
        // For each clause that's an INSERT, map column names → target positions.
        let mut delete_indices: Vec<usize> = Vec::new();
        let mut updates: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut inserts: Vec<Vec<Value>> = Vec::new();
        let mut affected: usize = 0;

        for (src_idx, src_row) in source_rows.iter().enumerate() {
            if src_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            // Find every matched target index (per the ON predicate).
            let mut matched_targets: Vec<usize> = Vec::new();
            for (t_idx, t_row) in target_rows_snapshot.iter().enumerate() {
                let mut combined_vals = t_row.values.clone();
                combined_vals.extend(src_row.values.iter().cloned());
                let combined_row = Row::new(combined_vals);
                let cond = eval::eval_expr(&stmt.on, &combined_row, &combined_ctx)?;
                if matches!(cond, Value::Bool(true)) {
                    matched_targets.push(t_idx);
                }
            }
            let is_matched = !matched_targets.is_empty();
            // Pick the first WHEN clause whose kind agrees with
            // `is_matched` and whose AND condition (if any) holds.
            // AND condition for MATCHED: evaluated against the
            // first matched target row × source. For NOT MATCHED:
            // evaluated with target side NULL-padded.
            let fired_clause = stmt.clauses.iter().find(|c| {
                let kind_ok = match c.matched {
                    spg_sql::ast::MergeMatched::Matched => is_matched,
                    spg_sql::ast::MergeMatched::NotMatched => !is_matched,
                };
                if !kind_ok {
                    return false;
                }
                let Some(cond_expr) = &c.condition else {
                    return true;
                };
                let row = if is_matched {
                    let t = &target_rows_snapshot[matched_targets[0]];
                    let mut vals = t.values.clone();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                } else {
                    let mut vals: Vec<Value> = (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                };
                let ctx_ref = if is_matched {
                    &combined_ctx
                } else {
                    &source_only_ctx
                };
                matches!(
                    eval::eval_expr(cond_expr, &row, ctx_ref),
                    Ok(Value::Bool(true))
                )
            });
            let Some(clause) = fired_clause else { continue };
            match &clause.action {
                spg_sql::ast::MergeAction::DoNothing => {}
                spg_sql::ast::MergeAction::Delete => {
                    for &t_idx in &matched_targets {
                        if !delete_indices.contains(&t_idx) {
                            delete_indices.push(t_idx);
                            affected += 1;
                        }
                    }
                }
                spg_sql::ast::MergeAction::Update { assignments } => {
                    // Pre-resolve SET targets to target column positions.
                    let mut planned_sets: Vec<(usize, &Expr)> =
                        Vec::with_capacity(assignments.len());
                    for (col, expr) in assignments {
                        let pos =
                            target_cols
                                .iter()
                                .position(|c| c.name == *col)
                                .ok_or_else(|| {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                })?;
                        planned_sets.push((pos, expr));
                    }
                    for &t_idx in &matched_targets {
                        let t_row = &target_rows_snapshot[t_idx];
                        let mut new_values = t_row.values.clone();
                        let mut combined_vals = t_row.values.clone();
                        combined_vals.extend(src_row.values.iter().cloned());
                        let combined_row = Row::new(combined_vals);
                        for (pos, expr) in &planned_sets {
                            let raw = eval::eval_expr(expr, &combined_row, &combined_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[*pos].ty,
                                &target_cols[*pos].name,
                                *pos,
                            )?;
                            new_values[*pos] = coerced;
                        }
                        updates.push((t_idx, new_values));
                        affected += 1;
                    }
                }
                spg_sql::ast::MergeAction::Insert { columns, values } => {
                    // For INSERT NOT MATCHED, target side is NULL-padded.
                    let mut vals: Vec<Value> = (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    let synth_row = Row::new(vals);
                    let mut new_row_values: Vec<Value> =
                        (0..target_arity).map(|_| Value::Null).collect();
                    for (col, expr) in columns.iter().zip(values.iter()) {
                        let pos =
                            target_cols
                                .iter()
                                .position(|c| c.name == *col)
                                .ok_or_else(|| {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                })?;
                        let raw = eval::eval_expr(expr, &synth_row, &source_only_ctx)?;
                        let coerced =
                            coerce_value(raw, target_cols[pos].ty, &target_cols[pos].name, pos)?;
                        new_row_values[pos] = coerced;
                    }
                    inserts.push(new_row_values);
                    affected += 1;
                }
            }
        }
        let _ = source_arity; // captured for symmetry; cancellation cost negligible.

        // Apply the plan to the target table.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.target)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
        // Apply updates first (in-place), then deletes (one batch),
        // then inserts. The storage API uses `update_row(pos,
        // new_values)`, `delete_rows(&[positions])`, and `insert(row)`.
        for (idx, new_vals) in &updates {
            table
                .update_row(*idx, new_vals.clone())
                .map_err(EngineError::Storage)?;
        }
        if !delete_indices.is_empty() {
            table.delete_rows(&delete_indices);
        }
        for vals in inserts {
            table.insert(Row::new(vals)).map_err(EngineError::Storage)?;
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: affected > 0,
        })
    }

    pub(crate) fn exec_delete_cancel(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.12.5 — snapshot BEFORE/AFTER DELETE row triggers + the
        // session FTS config before the mut borrow (same shape as
        // INSERT / UPDATE).
        let before_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "BEFORE");
        let after_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: PK-targeted DELETE → first retire any cold-tier
        // locator for the key. The cold row body stays in the
        // segment (becoming shadowed garbage that a future
        // compaction pass reclaims) but the index no longer
        // resolves it. The shadow count contributes to the
        // affected total; the subsequent hot walk handles any hot
        // rows for the same key.
        let mut cold_shadow_count: usize = 0;
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                cold_shadow_count = self
                    .active_catalog_mut()
                    .shadow_cold_row(&stmt.table, &idx_name, &key)
                    .unwrap_or(0);
            }
        }

        // v7.12.1 — cache the session FTS config as an owned
        // String before the mutable table borrow below; the
        // ctx-builder then references it via `as_deref` so the
        // immutable read of `session_params` doesn't conflict
        // with the mut borrow chain.
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()))
            .with_default_text_search_config(ts_cfg.as_deref());
        let mut positions: Vec<usize> = Vec::new();
        // v7.6.3 — collect every to-delete row's full Value tuple
        // alongside its position, so the FK enforcement pass can
        // run after the mut borrow drops.
        let mut to_delete_rows: Vec<Vec<Value>> = Vec::new();
        // v7.20 P4 — index seek (same shape as exec_update_cancel):
        // an equality WHERE on an indexed column narrows the walk
        // to the matching hot positions; the full WHERE still
        // re-evaluates per candidate. Downstream passes assume
        // ascending position order, so the seek result is sorted.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let candidate_positions: Vec<usize> = match seek_positions {
            Some(mut list) => {
                list.sort_unstable();
                list
            }
            None => (0..table.row_count()).collect(),
        };
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            let keep = if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                !matches!(cond, Value::Bool(true))
            } else {
                false
            };
            if !keep {
                positions.push(i);
                to_delete_rows.push(row.values.clone());
            }
        }
        // v7.6.3 / v7.6.4 — Stage 2: FK enforcement on the immutable
        // catalog. Release the mut borrow and run reverse-scan
        // against every child table whose FK targets this table.
        // RESTRICT / NoAction raise an error; CASCADE returns a
        // cascade plan that stage 3 applies after the primary delete.
        // SET NULL / SET DEFAULT remain Unsupported until v7.6.5.
        let _ = table;
        // v7.12.5 — BEFORE DELETE row-level triggers. Each fires
        // with NEW=None / OLD=pre-delete row; RETURN OLD (or NEW)
        // = proceed, RETURN NULL = skip the row entirely. The
        // filter must run BEFORE the FK cascade plan so cascaded
        // child rows track the trigger's skip-decision on the
        // parent.
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        if !before_delete_triggers.is_empty() {
            let mut filtered_positions: Vec<usize> = Vec::with_capacity(positions.len());
            let mut filtered_old_rows: Vec<Vec<Value>> = Vec::with_capacity(to_delete_rows.len());
            for (pos, old_vals) in positions.iter().zip(to_delete_rows.iter()) {
                let old_row = Row::new(old_vals.clone());
                let mut cancel_this = false;
                for fd in &before_delete_triggers {
                    let (outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        false,
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                    if matches!(outcome, triggers::TriggerOutcome::Skip) {
                        cancel_this = true;
                        break;
                    }
                }
                if !cancel_this {
                    filtered_positions.push(*pos);
                    filtered_old_rows.push(old_vals.clone());
                }
            }
            positions = filtered_positions;
            to_delete_rows = filtered_old_rows;
        }
        let cascade_plan = plan_fk_parent_deletions(
            self.active_catalog(),
            &stmt.table,
            &positions,
            &to_delete_rows,
        )?;
        // Stage 3a — apply each FK child step (SET NULL / SET
        // DEFAULT / CASCADE delete) before deleting the parent.
        // The plan is already ordered: nulls/defaults first, then
        // cascade deletes (so a row mutated and later deleted
        // surfaces as deleted — though v7.6.5 doesn't produce
        // that overlap today).
        for step in &cascade_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // Stage 3b — actually delete the original target rows.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let affected = table.delete_rows(&positions) + cold_shadow_count;
        let _ = table;
        // v7.12.5 — AFTER DELETE row-level triggers fire post-write
        // with NEW=None / OLD=pre-delete row (each from the
        // already-snapshotted to_delete_rows). Return value is
        // ignored (matches PG AFTER semantics).
        if !after_delete_triggers.is_empty() {
            for old_vals in &to_delete_rows {
                let old_row = Row::new(old_vals.clone());
                for fd in &after_delete_triggers {
                    let (_outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        true,
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                }
            }
        }
        // v7.12.7 — drain trigger-emitted embedded SQL for this DELETE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for DELETE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.9.4 — RETURNING projection over the soon-to-be-gone
        // rows. `to_delete_rows` was snapshotted in stage 1 before
        // mutation, so the projection sees the pre-delete state
        // (matches PG semantics: DELETE RETURNING returns the row
        // as it was just before removal).
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, to_delete_rows);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// Snapshot the per-INSERT catalog state the row loop depends on,
    /// taken while the catalog is still immutably borrowable (before the
    /// `get_mut` window): the clock fn, BEFORE/AFTER row triggers + their
    /// session config, the column ENUM / SET variant lookups, and the
    /// AUTO_INCREMENT sequence floors.
    fn prepare_insert_snapshots(&self, table_name: &str) -> Result<InsertSnapshots, EngineError> {
        // v7.9.21 — snapshot the clock fn pointer before the mut
        // borrow on the catalog opens; runtime DEFAULT eval needs
        // it inside the row hot loop.
        let clock = self.clock;
        // v7.12.4 — snapshot row-level triggers + their referenced
        // functions before the mut borrow on the catalog opens.
        // Cloned out so the row hot loop can fire them without
        // re-borrowing the catalog (which would conflict with
        // table.insert's mutable borrow).
        let before_insert_triggers = self.snapshot_row_triggers(table_name, "INSERT", "BEFORE");
        let after_insert_triggers = self.snapshot_row_triggers(table_name, "INSERT", "AFTER");
        let trigger_session_cfg: Option<alloc::string::String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v7.17.0 Phase 1.4 — snapshot the enum label lookup BEFORE
        // opening the mutable borrow on the table below. We need
        // catalog-level read access (enum_types lives at the
        // catalog level, not the table) and the upcoming mutable
        // borrow shadows it.
        let pre_borrow_column_meta: Vec<ColumnSchema> = {
            let preview_table = self.active_catalog().get(table_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: String::from(table_name),
                })
            })?;
            preview_table.schema().columns.clone()
        };
        let enum_label_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM
                    // variant lists take priority over the PG
                    // catalog enum_types lookup (they're
                    // column-local and authoritative when set).
                    if let Some(inline) = &col.inline_enum_variants {
                        return Some((i, inline.clone()));
                    }
                    col.user_enum_type.as_ref().and_then(|ename| {
                        self.active_catalog()
                            .enum_types()
                            .get(ename)
                            .map(|e| (i, e.labels.clone()))
                    })
                })
                .collect();
        // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant lists.
        // Distinct from enum_label_lookup: SET validates that
        // every comma-separated token is in the variant list, and
        // canonicalises the cell to definition-order de-duped text.
        let set_variant_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| col.inline_set_variants.as_ref().map(|vs| (i, vs.clone())))
                .collect();
        // v7.29 (round-23a) - when the column's implicit sequence
        // exists (born on first nextval/setval address), a setval
        // above the table MAX moves the next auto-assigned id:
        // assign from max(table_max + 1, last_value + 1). Tables
        // whose sequence was never addressed keep the bare max+1
        // path (identical pre-7.29 behaviour, no lookup cost
        // beyond one map probe per auto column per statement).
        let mut seq_floors: alloc::collections::BTreeMap<usize, i64> =
            alloc::collections::BTreeMap::new();
        for (i, col) in pre_borrow_column_meta.iter().enumerate() {
            if col.auto_increment
                && let Some(sd) = self.active_catalog().sequences().get(&alloc::format!(
                    "{}_{}_seq",
                    table_name,
                    col.name
                ))
            {
                // is_called=false (fresh RESTART / setval(_, false))
                // means the NEXT value is last_value itself.
                let floor = if sd.is_called {
                    sd.last_value + 1
                } else {
                    sd.last_value
                };
                seq_floors.insert(i, floor);
            }
        }
        Ok(InsertSnapshots {
            clock,
            before_insert_triggers,
            after_insert_triggers,
            trigger_session_cfg,
            enum_label_lookup,
            set_variant_lookup,
            seq_floors,
        })
    }

    pub(crate) fn exec_insert(
        &mut self,
        mut stmt: InsertStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.17.0 Phase 1.1 — pre-resolve any nextval / currval /
        // setval calls against the catalog before the row loop. We
        // walk each tuple expression and replace matching
        // FunctionCall nodes with their concrete Literal. This
        // keeps `literal_expr_to_value` free of `&mut self` and
        // lets multi-row INSERT VALUES (… nextval('seq') …)
        // mint a separate sequence value per row.
        for tuple in &mut stmt.rows {
            for cell in tuple.iter_mut() {
                self.resolve_sequence_calls_in_expr(cell)?;
            }
        }
        // v7.13.0 — `INSERT INTO t [(cols)] SELECT …` (mailrs
        // round-5 G4). Execute the inner SELECT first, then route
        // back through the regular VALUES code path with the
        // materialised rows.
        if let Some(select) = stmt.select_source.clone() {
            let select_result = self.exec_select_cancel(&select, CancelToken::none())?;
            let rows = match select_result {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "INSERT … SELECT: inner statement produced {other:?} instead of a row set"
                    )));
                }
            };
            let mut materialised: Vec<Vec<Expr>> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut tuple: Vec<Expr> = Vec::with_capacity(row.values.len());
                for v in row.values {
                    tuple.push(value_to_literal_expr_permissive(v)?);
                }
                materialised.push(tuple);
            }
            let recurse = InsertStatement {
                table: stmt.table,
                columns: stmt.columns,
                rows: materialised,
                select_source: None,
                on_conflict: stmt.on_conflict,
                returning: stmt.returning,
            };
            return self.exec_insert(recurse);
        }
        // Snapshot everything the row loop needs from the catalog
        // before the mutable borrow below shadows it (clock, triggers,
        // enum/set variant lookups, sequence floors).
        let InsertSnapshots {
            clock,
            before_insert_triggers,
            after_insert_triggers,
            trigger_session_cfg,
            enum_label_lookup,
            set_variant_lookup,
            seq_floors,
        } = self.prepare_insert_snapshots(&stmt.table)?;
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v3.1.5: clone the columns vector only (not the whole
        // TableSchema — saves one String alloc for the table name).
        // We need an owned snapshot because we'll call `table.insert`
        // (mutable borrow on `table`) inside the row loop while
        // reading schema fields.
        let column_meta: Vec<ColumnSchema> = table.schema().columns.clone();
        let schema_cols_len = column_meta.len();
        let tuple_pos = build_tuple_pos(&stmt.columns, &column_meta)?;
        let expected_tuple_len = stmt.columns.as_ref().map_or(schema_cols_len, Vec::len);
        // v7.6.2 — snapshot this table's FK list before the
        // mutable-borrow window so we can run parent lookups
        // against the immutable catalog after parsing. Empty vec is
        // the no-FK fast path; clone cost is O(fks * arity) which
        // is < 100 ns for typical schemas.
        let fks = table.schema().foreign_keys.clone();
        // Stage 1 — parse + AUTO_INC + coerce all rows under the
        // (immutable) table borrow.
        let mut all_values = parse_insert_rows(
            table,
            stmt.rows,
            &column_meta,
            &tuple_pos,
            expected_tuple_len,
            clock,
            &seq_floors,
            &enum_label_lookup,
            &set_variant_lookup,
        )?;
        // Stage 2 — FK enforcement on the immutable catalog.
        // Non-lexical lifetimes release the mutable borrow on
        // `table` here since stage 1 was the last use. The
        // parent-table lookup runs before any row is committed.
        let uniqueness = table.schema().uniqueness_constraints.clone();
        let _ = table;
        if !fks.is_empty() {
            enforce_fk_inserts(self.active_catalog(), &stmt.table, &fks, &all_values)?;
        }
        // v7.13.0 — CHECK constraint enforcement (mailrs round-5 G3).
        enforce_check_constraints(self.active_catalog(), &stmt.table, &all_values)?;
        // NOTE (mailrs embed round-12): UNIQUE / PRIMARY KEY and
        // UNIQUE INDEX enforcement moved BELOW the ON CONFLICT
        // resolution pass. Running them first made every
        // `ON CONFLICT … DO UPDATE` upsert fail with a uniqueness
        // violation before the conflict handler could route the row
        // to an UPDATE — PG resolves the conflict action first and
        // only errors on rows no arbiter matched.
        // v7.9.8 / v7.9.9 — ON CONFLICT handling.
        //   - `DO NOTHING` filters `all_values` to non-conflicting
        //     rows + drops within-batch duplicates.
        //   - `DO UPDATE SET …` ALSO filters, but for each
        //     conflicting row it queues an UPDATE on the existing
        //     row using the incoming row's values as `EXCLUDED.*`.
        let (pending_updates, skipped_count) = match &stmt.on_conflict {
            Some(clause) => {
                let (kept, pending, skipped) =
                    self.resolve_insert_on_conflict(&stmt.table, clause, all_values)?;
                all_values = kept;
                (pending, skipped)
            }
            None => (Vec::new(), 0usize),
        };
        // v7.9.19 — composite UNIQUE / PRIMARY KEY enforcement.
        // v7.9.29 — CREATE UNIQUE INDEX [WHERE pred] enforcement.
        // Both run on the post-ON-CONFLICT row set: conflicting rows
        // already left `all_values` (DO NOTHING drop / DO UPDATE
        // reroute), so what remains must be genuinely unique.
        enforce_uniqueness_inserts(self.active_catalog(), &stmt.table, &uniqueness, &all_values)?;
        enforce_unique_index_inserts(self.active_catalog(), &stmt.table, &all_values)?;
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // Stage 3 — insert the surviving rows + fire row triggers under
        // a fresh mutable borrow, then apply queued ON CONFLICT updates.
        let (returning_rows, deferred_embedded, affected) = insert_parsed_rows(
            table,
            all_values,
            pending_updates,
            &before_insert_triggers,
            &after_insert_triggers,
            &column_meta,
            &stmt.table,
            trigger_session_cfg.as_deref(),
            stmt.returning.is_some(),
        )?;
        let _ = skipped_count;
        // v7.12.7 — drop the table mut borrow and drain any
        // trigger-emitted embedded SQL queued during this INSERT.
        // The borrow has to release first because each deferred
        // stmt may UPDATE / INSERT / DELETE the same (or another)
        // table — including, in principle, this one.
        let _ = table;
        self.execute_deferred_trigger_stmts(deferred_embedded, CancelToken::none())?;
        // v7.9.4/v7.9.9 — RETURNING streams the rows that ended
        // up in the table after this statement (insert or
        // post-update on conflict).
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, returning_rows);
        }
        // v6.2.1 — auto-analyze: track per-table modified-row
        // counter so the background sweep can decide when to
        // re-ANALYZE. Cheap path on the autocommit-wrap hot loop
        // — one BTreeMap entry update per INSERT batch.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// (ON CONFLICT) Resolve a `DO NOTHING` / `DO UPDATE` clause against
    /// the post-parse row set: drop or reroute conflicting rows, queue
    /// updates for the existing rows they collide with, and return the
    /// rows that survive to be inserted plus those queued updates.
    #[allow(clippy::type_complexity)]
    fn resolve_insert_on_conflict(
        &self,
        table_name: &str,
        clause: &spg_sql::ast::OnConflictClause,
        all_values: Vec<Vec<Value>>,
    ) -> Result<(Vec<Vec<Value>>, Vec<(usize, Vec<Value>)>, usize), EngineError> {
        let mut pending_updates: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut skipped_count = 0usize;
        let (conflict_cols, conflict_nnd) = resolve_on_conflict_columns(
            self.active_catalog(),
            table_name,
            clause.target_columns.as_slice(),
        )?;
        let mut kept: Vec<Vec<Value>> = Vec::with_capacity(all_values.len());
        let mut seen_keys: Vec<Vec<Value>> = Vec::new();
        for values in all_values {
            let key_tuple: Vec<&Value> = conflict_cols.iter().map(|&c| &values[c]).collect();
            // SQL spec: NULL in any conflict column means "no
            // conflict possible" (NULL ≠ NULL for uniqueness) —
            // UNLESS the constraint says NULLS NOT DISTINCT
            // (v7.29; mailrs migrate-013 replays its seed row
            // ('super', NULL) under exactly that declaration).
            let has_null_key = !conflict_nnd && key_tuple.iter().any(|v| matches!(v, Value::Null));
            let collides_with_table = !has_null_key
                && on_conflict_keys_exist(
                    self.active_catalog(),
                    table_name,
                    &conflict_cols,
                    &key_tuple,
                );
            let key_tuple_owned: Vec<Value> = key_tuple.iter().map(|v| (*v).clone()).collect();
            let collides_with_batch =
                !has_null_key && seen_keys.iter().any(|k| k == &key_tuple_owned);
            let collides = collides_with_table || collides_with_batch;
            match (&clause.action, collides) {
                (_, false) => {
                    seen_keys.push(key_tuple_owned);
                    kept.push(values);
                }
                (spg_sql::ast::OnConflictAction::Nothing, true) => {
                    skipped_count += 1;
                }
                (
                    spg_sql::ast::OnConflictAction::Update {
                        assignments,
                        where_,
                    },
                    true,
                ) => {
                    if !collides_with_table {
                        skipped_count += 1;
                        continue;
                    }
                    let target_pos = lookup_row_position_by_keys(
                        self.active_catalog(),
                        table_name,
                        &conflict_cols,
                        &key_tuple,
                    )
                    .ok_or_else(|| {
                        EngineError::Unsupported(
                            "ON CONFLICT DO UPDATE: conflict detected but row \
                                 position could not be resolved (cold-tier row?)"
                                .into(),
                        )
                    })?;
                    let updated = apply_on_conflict_assignments(
                        self.active_catalog(),
                        table_name,
                        target_pos,
                        &values,
                        assignments,
                        where_.as_ref(),
                    )?;
                    if let Some(new_row) = updated {
                        pending_updates.push((target_pos, new_row));
                    } else {
                        skipped_count += 1;
                    }
                }
            }
        }
        Ok((kept, pending_updates, skipped_count))
    }
}

impl Engine {
    /// v7.9.4 — INSERT / UPDATE / DELETE RETURNING projector.
    /// Given the table name, the user-supplied projection items,
    /// and the mutated rows (post-insert / post-update values, or
    /// pre-delete snapshot), build a `QueryResult::Rows` whose
    /// schema describes the projected columns. Mailrs migration
    /// blocker #1.
    fn build_returning_rows(
        &self,
        table_name: &str,
        items: &[SelectItem],
        mutated_rows: Vec<Vec<Value>>,
    ) -> Result<QueryResult, EngineError> {
        let table = self.active_catalog().get(table_name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: table_name.into(),
            })
        })?;
        let schema_cols = table.schema().columns.clone();
        let columns = self.derive_output_columns(items, &schema_cols, table_name);
        let mut out_rows: Vec<Row> = Vec::with_capacity(mutated_rows.len());
        for values in mutated_rows {
            let row = Row::new(values);
            let projected = self.project_row_simple(&row, items, &schema_cols, table_name)?;
            out_rows.push(projected);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }
}

/// Build the INSERT column permutation `tuple_pos[c] = Some(j)` (schema
/// column `c` is filled from the `j`-th tuple slot; `None` = fill with
/// NULL / DEFAULT). `None` overall means the 1-1 fast path. Validates
/// the column list once for reuse across every row.
fn build_tuple_pos(
    columns: &Option<Vec<String>>,
    column_meta: &[ColumnSchema],
) -> Result<Option<Vec<Option<usize>>>, EngineError> {
    let schema_cols_len = column_meta.len();
    // Build a permutation `tuple_pos[c] = Some(j)` meaning schema
    // column `c` is filled from the `j`-th tuple slot; `None` means
    // "fill with NULL". Validated once and reused for every row.
    let tuple_pos: Option<Vec<Option<usize>>> = match columns {
        None => None, // 1-1 mapping, fast path
        Some(cols) => {
            let mut map = alloc::vec![None; schema_cols_len];
            for (j, name) in cols.iter().enumerate() {
                let idx = column_meta
                    .iter()
                    .position(|c| c.name == *name)
                    .ok_or_else(|| {
                        EngineError::Eval(EvalError::ColumnNotFound { name: name.clone() })
                    })?;
                if map[idx].is_some() {
                    return Err(EngineError::Storage(StorageError::ArityMismatch {
                        expected: schema_cols_len,
                        actual: cols.len(),
                    }));
                }
                map[idx] = Some(j);
            }
            // Omitted columns must either be nullable, carry a
            // DEFAULT, or be AUTO_INCREMENT. Catch NOT NULL
            // omissions up front so the WAL stays clean.
            for (i, col) in column_meta.iter().enumerate() {
                if map[i].is_none()
                    && !col.nullable
                    && col.default.is_none()
                    && col.runtime_default.is_none()
                    && !col.auto_increment
                {
                    return Err(EngineError::Storage(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    }));
                }
            }
            Some(map)
        }
    };
    Ok(tuple_pos)
}

/// Stage 1 — parse every INSERT tuple into a coerced row of `Value`s:
/// apply the column permutation, mint AUTO_INCREMENT ids (statement-
/// scoped cursors), run DEFAULT / ENUM / SET / unsigned-range checks.
/// Reads the table immutably (`next_auto_value`); no row is written yet.
#[allow(clippy::too_many_arguments)]
fn parse_insert_rows(
    table: &spg_storage::Table,
    rows: Vec<Vec<Expr>>,
    column_meta: &[ColumnSchema],
    tuple_pos: &Option<Vec<Option<usize>>>,
    expected_tuple_len: usize,
    clock: Option<crate::ClockFn>,
    seq_floors: &alloc::collections::BTreeMap<usize, i64>,
    enum_label_lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
    set_variant_lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
) -> Result<Vec<Vec<Value>>, EngineError> {
    let schema_cols_len = column_meta.len();
    let mut all_values: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    // v7.24 (round-16 collateral) — statement-scoped serial
    // cursors. next_auto_value() is a max+1 scan over COMMITTED
    // rows; multi-row `INSERT … VALUES (…),(…)` computed it per
    // tuple BEFORE any insertion, so every row drew the SAME id
    // (then sailed through, compounding with the inline-PK
    // enforcement gap). First use per column seeds from the
    // table; subsequent rows increment.
    let mut auto_cursors: alloc::collections::BTreeMap<usize, i64> =
        alloc::collections::BTreeMap::new();
    for tuple in rows {
        if tuple.len() != expected_tuple_len {
            return Err(EngineError::Storage(StorageError::ArityMismatch {
                expected: expected_tuple_len,
                actual: tuple.len(),
            }));
        }
        // Fast path: no column-list permutation → tuple slot j
        // maps to schema column j. We can zip schema with tuple
        // and skip the `raw_tuple` staging allocation entirely.
        let values: Vec<Value> = if let Some(map) = &tuple_pos {
            // Permuted path: still need raw_tuple to index by `map[i]`.
            let raw_tuple: Vec<Value> = tuple
                .into_iter()
                .map(literal_expr_to_value)
                .collect::<Result<_, _>>()?;
            let mut out = Vec::with_capacity(schema_cols_len);
            for (i, col) in column_meta.iter().enumerate() {
                let mut raw = match map[i] {
                    Some(j) => raw_tuple[j].clone(),
                    None => resolve_column_default_free(col, clock)?,
                };
                if col.auto_increment && raw.is_null() {
                    let next = match auto_cursors.get(&i) {
                        Some(n) => *n,
                        None => {
                            let base = table.next_auto_value(i).ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                    col.name
                                ))
                            })?;
                            base.max(seq_floors.get(&i).copied().unwrap_or(i64::MIN))
                        }
                    };
                    auto_cursors.insert(i, next + 1);
                    raw = Value::BigInt(next);
                }
                let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                enforce_enum_label(enum_label_lookup, i, &col.name, &coerced)?;
                let coerced = canonicalize_set_value(set_variant_lookup, i, &col.name, coerced)?;
                check_unsigned_range(&coerced, col, i)?;
                out.push(coerced);
            }
            out
        } else {
            // 1-1 mapping fast path: single Vec alloc, no raw_tuple.
            let mut out = Vec::with_capacity(schema_cols_len);
            for (i, (col, expr)) in column_meta.iter().zip(tuple).enumerate() {
                let mut raw = literal_expr_to_value(expr)?;
                if col.auto_increment && raw.is_null() {
                    let next = match auto_cursors.get(&i) {
                        Some(n) => *n,
                        None => {
                            let base = table.next_auto_value(i).ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                    col.name
                                ))
                            })?;
                            base.max(seq_floors.get(&i).copied().unwrap_or(i64::MIN))
                        }
                    };
                    auto_cursors.insert(i, next + 1);
                    raw = Value::BigInt(next);
                }
                let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                enforce_enum_label(enum_label_lookup, i, &col.name, &coerced)?;
                let coerced = canonicalize_set_value(set_variant_lookup, i, &col.name, coerced)?;
                check_unsigned_range(&coerced, col, i)?;
                out.push(coerced);
            }
            out
        };
        all_values.push(values);
    }
    Ok(all_values)
}

/// Stage 3 — insert the surviving rows under a mutable table borrow,
/// firing BEFORE / AFTER row triggers (which may rewrite or skip a row
/// and emit deferred embedded SQL), then apply the queued ON CONFLICT
/// DO UPDATE rewrites. Returns the RETURNING projection rows, the
/// deferred trigger statements, and the affected-row count.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn insert_parsed_rows(
    table: &mut spg_storage::Table,
    all_values: Vec<Vec<Value>>,
    pending_updates: Vec<(usize, Vec<Value>)>,
    before_insert_triggers: &[spg_storage::FunctionDef],
    after_insert_triggers: &[spg_storage::FunctionDef],
    column_meta: &[ColumnSchema],
    table_name: &str,
    trigger_session_cfg: Option<&str>,
    returning_enabled: bool,
) -> Result<(Vec<Vec<Value>>, Vec<triggers::DeferredEmbeddedStmt>, usize), EngineError> {
    let mut affected = 0usize;
    // v7.9.4 — keep RETURNING projection rows separate per
    // INSERT and per UPDATE branch so DO UPDATE pushes the new
    // post-update state, not the incoming-only values.
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    // v7.12.7 — collect embedded SQL emitted by any trigger
    // fire across the row loop; engine drains the queue after
    // the table mut borrow drops.
    let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
    'rowloop: for values in all_values {
        let mut row = Row::new(values);
        // v7.12.4 — BEFORE INSERT row-level triggers. Each
        // trigger may rewrite NEW cells (e.g. populate
        // `search_vector := to_tsvector(...)`) and may return
        // NULL to skip the row entirely.
        for fd in before_insert_triggers {
            let (outcome, deferred) = triggers::fire_row_trigger(
                fd,
                Some(row.clone()),
                None,
                table_name,
                column_meta,
                &[],
                trigger_session_cfg,
                false,
            )
            .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
            deferred_embedded.extend(deferred);
            match outcome {
                triggers::TriggerOutcome::Row(r) => row = r,
                triggers::TriggerOutcome::Skip => continue 'rowloop,
            }
        }
        if returning_enabled {
            returning_rows.push(row.values.clone());
        }
        // v7.12.4 — clone for the AFTER trigger view; insert
        // moves the row into the table.
        let inserted = row.clone();
        table.insert(row)?;
        affected += 1;
        // v7.12.4 — AFTER INSERT row-level triggers fire post-
        // write. Return value is ignored (PG semantics); we
        // surface any error from the body up to the caller.
        for fd in after_insert_triggers {
            let (_outcome, deferred) = triggers::fire_row_trigger(
                fd,
                Some(inserted.clone()),
                None,
                table_name,
                column_meta,
                &[],
                trigger_session_cfg,
                true,
            )
            .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
            deferred_embedded.extend(deferred);
        }
    }
    // v7.9.9 — apply ON CONFLICT DO UPDATE rewrites collected
    // in the conflict-resolution pass. update_row handles
    // index maintenance + body re-encoding.
    for (pos, new_row) in pending_updates {
        if returning_enabled {
            returning_rows.push(new_row.clone());
        }
        table.update_row(pos, new_row)?;
        affected += 1;
    }
    Ok((returning_rows, deferred_embedded, affected))
}
