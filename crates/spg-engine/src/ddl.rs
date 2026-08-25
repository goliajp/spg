//! DDL execution — every CREATE / DROP / ALTER for schema objects:
//! tables and indexes, plus users, functions, triggers, sequences,
//! views, types, domains, schemas, and materialized views. Lifted out
//! of `lib.rs` (v7.32 engine modularisation). These `impl Engine`
//! methods are dispatched from `Engine::execute` (hence pub(crate)) and
//! drive the catalog / storage schema mutations.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{
    ColumnDef, CreateIndexStatement, CreateTableStatement, CreateUserStatement, Expr, IndexMethod,
    Literal, PartitionKindAst, PartitionOfBoundsAst, Statement, VecEncoding as SqlVecEncoding,
};
use spg_storage::{
    ColumnSchema, DataType, ExclusionConstraint, PartitionKind, PartitionRole, RangeKind,
    StorageError, TableSchema, Value, VecEncoding,
};

/// v7.39 (round 215) — the column an EXCLUDE constraint's range-overlap index
/// should key on: the `&&` element sitting on an integer-keyable range column
/// (int4/int8/date/ts/tstz range — the kinds `range_excl_index_key` reduces to
/// an `i128`). `None` when no element qualifies (numrange, or a non-`&&`
/// operator only), in which case the constraint keeps the O(n) enforcement.
fn excl_index_column(schema: &TableSchema, ex: &ExclusionConstraint) -> Option<usize> {
    for (pos, op) in &ex.elements {
        if op == "&&"
            && let Some(col) = schema.columns.get(*pos)
            && matches!(
                col.ty,
                DataType::Range(
                    RangeKind::Int4
                        | RangeKind::Int8
                        | RangeKind::Date
                        | RangeKind::Ts
                        | RangeKind::TsTz
                )
            )
        {
            return Some(*pos);
        }
    }
    None
}

/// v7.39 (round 215) — rebuild the range-exclusion indexes for every table in
/// a freshly-deserialized catalog. The indexes aren't persisted (like BRIN,
/// they re-derive), so a catalog load must re-emit them from the persisted
/// exclusion constraints + rows before the first EXCLUDE enforcement runs.
pub(crate) fn rebuild_all_excl_indexes(cat: &mut spg_storage::Catalog) {
    for name in cat.table_names() {
        let Some(table) = cat.get_mut(&name) else {
            continue;
        };
        let cols: Vec<usize> = table
            .schema()
            .exclusion_constraints
            .iter()
            .filter_map(|ex| excl_index_column(table.schema(), ex))
            .collect();
        for c in cols {
            table.ensure_excl_range_index(c);
        }
    }
}

use crate::{
    CancelToken, ClockFn, Engine, EngineError, QueryResult, check_existing_unique_violation,
    coerce_value, column_type_to_data_type, enforce_fk_inserts, eval, infer_column_types,
    literal_expr_to_value, resolve_foreign_key, rewrite_column_in_source, users,
};

/// v7.39 (round 475) — the column a `to_tsvector(…)` index key reads.
///
/// PG's full-text idiom is `CREATE INDEX … USING gin (to_tsvector('simple',
/// body))`, and it is the reason a PG schema reaches the expression path at
/// all. SPG already builds a fulltext GIN over a column for MySQL's
/// `FULLTEXT KEY`; this recognises the shape so the PG spelling lands on the
/// same index instead of being refused.
///
/// `None` for anything else, including `to_tsvector` over an expression
/// rather than a bare column — indexing a derived value is a different
/// build, and guessing at it would be worse than refusing.
fn tsvector_source_column(e: &spg_sql::ast::Expr) -> Option<String> {
    let spg_sql::ast::Expr::FunctionCall { name, args } = e else {
        return None;
    };
    if !name.eq_ignore_ascii_case("to_tsvector") {
        return None;
    }
    // `to_tsvector(col)` or `to_tsvector(config, col)` — either way the
    // column is the last argument.
    match args.last() {
        Some(spg_sql::ast::Expr::Column(c)) => Some(c.name.clone()),
        _ => None,
    }
}

impl Engine {
    /// v6.7.2 — `ALTER TABLE t SET hot_tier_bytes = X`. Dispatch
    /// arm. Currently the only setting is `hot_tier_bytes`; later
    /// v6.7.x can extend `AlterTableTarget` without touching this
    /// arm structure.
    pub(crate) fn exec_alter_table(
        &mut self,
        s: spg_sql::ast::AlterTableStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.13.2 — mailrs round-6 S1: apply each subaction in order.
        // On first error the statement aborts; subactions already
        // applied stay (no transactional rollback in v7.13 — wrap in
        // BEGIN/COMMIT if atomicity matters).
        let table_name = s.name.clone();
        // v7.39 (round 735, S14/B3) — any table-shape change invalidates
        // a dependent materialized view's refresh watermark.
        self.bump_table_change(&table_name);
        for target in s.targets {
            self.exec_alter_table_subaction(&table_name, target)?;
        }
        // v7.39 (round 215) — (re)build range-exclusion indexes after any
        // ALTER: ADD EXCLUDE installs a new one; DROP COLUMN cleared them (it
        // shifts positions), so this restores them from the constraints'
        // updated column positions. Idempotent for the untouched case.
        self.install_excl_range_indexes(&table_name);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    pub(crate) fn exec_alter_table_subaction(
        &mut self,
        table_name_outer: &str,
        target: spg_sql::ast::AlterTableTarget,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::AlterTableTarget as T;
        let tbl = table_name_outer;
        match target {
            // v7.39 (round 647) — attach or detach an inheritance child.
            // Accepted-and-ignored since v7.37.18, whose reasoning ("SPG
            // doesn't support PG-style inheritance") round 645 made
            // false. `NO INHERIT` reporting success while the child
            // stayed attached is the worst shape a statement can have.
            T::Inherit { parent, detach } => self.alter_inherit(tbl, &parent, detach),
            T::SetHotTierBytes(n) => self.alter_set_hot_tier_bytes(tbl, n),
            T::AddForeignKey(fk) => self.alter_add_foreign_key(tbl, fk),
            T::DropForeignKey { name, if_exists } => {
                self.alter_drop_foreign_key(tbl, name, if_exists)
            }
            // v7.39 (round 431) — `ALTER TABLE t DROP {INDEX|KEY} name`
            // shares the standalone DROP INDEX path, so the two spellings
            // cannot diverge on the not-found / IF EXISTS behaviour.
            T::DropIndex { name, if_exists } => self.exec_drop_index(name, if_exists).map(|_| ()),
            T::AddColumn {
                column,
                if_not_exists,
            } => self.alter_add_column(tbl, column, if_not_exists),
            T::AlterColumnType {
                column,
                new_type,
                using,
                collation,
            } => self.alter_column_type(tbl, column, new_type, using, collation),
            T::AddTableConstraint(tc) => self.alter_add_table_constraint(tbl, tc),
            T::ValidateConstraint { name } => self.alter_validate_constraint(tbl, &name),
            // v7.39 (round 652) — SPG is single-owner and has no
            // clustered storage, so both of these remain no-ops once the
            // name checks out. What was missing was the check.
            T::OwnerTo { role } => {
                if self.role_exists(&role) {
                    Ok(())
                } else {
                    Err(EngineError::Unsupported(alloc::format!(
                        "role \"{role}\" does not exist"
                    )))
                }
            }
            // v7.39 (round 710) — same shape as OwnerTo/ClusterOn above:
            // the ACTION no-ops, the NAME check is what was missing.
            T::OfType { type_name } => {
                let cat = self.active_catalog();
                if cat.enum_types().contains_key(&type_name)
                    || cat.domain_types().contains_key(&type_name)
                    || cat.composite_types().contains_key(&type_name)
                {
                    Ok(())
                } else {
                    Err(EngineError::Unsupported(alloc::format!(
                        "type \"{type_name}\" does not exist"
                    )))
                }
            }
            T::ReplicaIdentityUsingIndex { index } => {
                let table = self.active_catalog().get(tbl).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
                })?;
                if table
                    .indices()
                    .iter()
                    .any(|i| i.name.eq_ignore_ascii_case(&index))
                {
                    Ok(())
                } else {
                    Err(EngineError::Unsupported(alloc::format!(
                        "index \"{index}\" for table \"{tbl}\" does not exist"
                    )))
                }
            }
            T::ClusterOn { index } => {
                let Some(index) = index else { return Ok(()) };
                let table = self.active_catalog().get(tbl).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
                })?;
                if table
                    .indices()
                    .iter()
                    .any(|i| i.name.eq_ignore_ascii_case(&index))
                {
                    Ok(())
                } else {
                    Err(EngineError::Unsupported(alloc::format!(
                        "index \"{index}\" for table \"{tbl}\" does not exist"
                    )))
                }
            }
            T::DropColumn {
                column,
                if_exists,
                cascade,
            } => self.alter_drop_column(tbl, column, if_exists, cascade),
            T::SetTriggerEnabled { which, enabled } => {
                self.alter_set_trigger_enabled(tbl, which, enabled)
            }
            T::SetColumnAutoIncrement { column, seq_name } => {
                self.alter_set_column_auto_increment(tbl, column, seq_name)
            }
            T::RenameTable { new } => self.alter_rename_table(tbl, new),
            T::RenameColumn { old, new } => self.alter_rename_column(tbl, old, new),
            T::RenameConstraint { old, new } => self.alter_rename_constraint(tbl, &old, new),
            T::AttachPartition { child, bounds } => self.alter_attach_partition(tbl, child, bounds),
            T::DetachPartition {
                child,
                concurrently,
                finalize,
            } => self.alter_detach_partition(tbl, child, concurrently, finalize),
            T::AlterColumnSetDefault {
                column,
                default_expr,
            } => self.alter_column_set_default(tbl, column, default_expr),
            T::AlterColumnDropDefault { column } => self.alter_column_drop_default(tbl, column),
            T::AlterColumnSetNotNull { column } => self.alter_column_set_not_null(tbl, column),
            T::AlterColumnDropNotNull { column } => self.alter_column_drop_not_null(tbl, column),
            // v7.39 (round 220) — RESTART [WITH n]: record the next-value
            // floor on the identity column (max+1 alloc takes the max).
            T::AlterColumnRestart { column, with } => {
                let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
                })?;
                let Some(col) = table
                    .schema_mut()
                    .columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(&column))
                else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "column \"{column}\" of relation \"{tbl}\" does not exist"
                    )));
                };
                col.auto_restart = Some(with.unwrap_or(1));
                Ok(())
            }
            T::AlterColumnDropExpression { column, if_exists } => {
                self.alter_column_drop_expression(tbl, column, if_exists)
            }
            T::AlterColumnDropIdentity { column, if_exists } => {
                self.alter_column_drop_identity(tbl, column, if_exists)
            }
            T::AlterColumnSetExpression { column, expr } => {
                self.alter_column_set_expression(tbl, column, expr)
            }
            T::SetRowSecurity { enabled, force } => {
                self.alter_set_row_security(tbl, enabled, force)
            }
        }
    }

    /// v7.39 (RLS) — `ALTER TABLE t { ENABLE|DISABLE|FORCE|NO FORCE } ROW LEVEL
    /// SECURITY`. Sets the schema flags (`relrowsecurity` / `relforcerowsecurity`
    /// mirrors). Enforcement is gated on the session role (Phase 1); Phase 0
    /// only records the flags for catalog / pg_dump fidelity.
    fn alter_set_row_security(
        &mut self,
        tbl: &str,
        enabled: Option<bool>,
        force: Option<bool>,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        if let Some(e) = enabled {
            table.schema_mut().row_security = e;
        }
        if let Some(fo) = force {
            table.schema_mut().force_row_security = fo;
        }
        Ok(())
    }

    /// v7.38 (read01 U12) — `ALTER COLUMN col SET EXPRESSION AS (expr)`
    /// (PG 17): swap a stored generated column's expression and recompute
    /// every existing row against the new expression.
    fn alter_column_set_expression(
        &mut self,
        tbl: &str,
        column: String,
        expr: spg_sql::ast::Expr,
    ) -> Result<(), EngineError> {
        let expr_str = alloc::format!("{expr}");
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN SET EXPRESSION: column {column:?} not in table {tbl:?}"
                ))
            })?;
        if table.schema().columns[pos].generated_stored_expr.is_none() {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER COLUMN SET EXPRESSION: column {column:?} is not a stored generated column"
            )));
        }
        table.schema_mut().columns[pos].generated_stored_expr = Some(expr_str);
        // Recompute existing rows against the new expression.
        let schema_cols = table.schema().columns.clone();
        let col_ty = schema_cols[pos].ty;
        let ctx = crate::eval::EvalContext::new(&schema_cols, None);
        let mut new_values: Vec<Value<'static>> = Vec::with_capacity(table.rows().len());
        for row in table.rows().iter() {
            let v = eval::eval_expr(&expr, row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN SET EXPRESSION: recompute failed: {e:?}"
                ))
            })?;
            new_values.push(coerce_value(v, col_ty, &column, pos)?);
        }
        for (i, v) in new_values.into_iter().enumerate() {
            let mut row_values = table
                .rows()
                .get(i)
                .expect("bounds-checked by the loop above")
                .values
                .clone();
            row_values[pos] = v;
            table.update_row(i, row_values)?;
        }
        Ok(())
    }

    /// v7.38 (read01 U10) — `ALTER COLUMN col DROP EXPRESSION` converts a
    /// stored generated column to a plain column: clear the generation
    /// expression so future INSERT/UPDATE accept a supplied value instead
    /// of recomputing it. Existing stored values are left as-is.
    fn alter_column_drop_expression(
        &mut self,
        tbl: &str,
        column: String,
        if_exists: bool,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN DROP EXPRESSION: column {column:?} not in table {tbl:?}"
                ))
            })?;
        if table.schema().columns[pos].generated_stored_expr.is_none() {
            // v7.39 (round 187, U10) — PG's wordings, live-verified
            // 2026-07-18: plain form errors, IF EXISTS raises a NOTICE
            // and skips (`ALTER TABLE` still succeeds — pg_dump
            // restore scripts rely on that).
            if if_exists {
                self.notice(alloc::format!(
                    "column \"{column}\" of relation \"{tbl}\" is not a generated column, skipping"
                ));
                return Ok(());
            }
            return Err(EngineError::Unsupported(alloc::format!(
                "column \"{column}\" of relation \"{tbl}\" is not a generated column"
            )));
        }
        table.schema_mut().columns[pos].generated_stored_expr = None;
        Ok(())
    }

    /// v7.38 (read01, T28) — `ALTER COLUMN col DROP IDENTITY [IF EXISTS]`:
    /// de-generate an identity column into a plain column. Errors when the
    /// column is not an identity column, unless `IF EXISTS` was given.
    fn alter_column_drop_identity(
        &mut self,
        tbl: &str,
        column: String,
        if_exists: bool,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN DROP IDENTITY: column {column:?} not in table {tbl:?}"
                ))
            })?;
        if !table.schema().columns[pos].auto_increment {
            if if_exists {
                return Ok(());
            }
            // PG18.4: `column "a" of relation "t3" is not an identity column`.
            return Err(EngineError::Unsupported(alloc::format!(
                "column {column:?} of relation {tbl:?} is not an identity column"
            )));
        }
        table.schema_mut().columns[pos].auto_increment = false;
        // v7.38 (read01) — a dropped identity is a plain column: clear the
        // ALWAYS marker too so explicit INSERT values are accepted again.
        table.schema_mut().columns[pos].identity_always = false;
        Ok(())
    }

    /// v7.37.18 (18.1) — set / drop column default.
    fn alter_column_set_default(
        &mut self,
        tbl: &str,
        column: String,
        default_expr: spg_sql::ast::Expr,
    ) -> Result<(), EngineError> {
        // Volatile defaults (now(), nextval(), …) go through the
        // runtime_default path; literal defaults freeze into `default`.
        let display = alloc::format!("{}", default_expr);
        let is_runtime = matches!(default_expr, spg_sql::ast::Expr::FunctionCall { .. });
        let literal_value = if is_runtime {
            None
        } else {
            crate::conversions::literal_expr_to_value(default_expr.clone()).ok()
        };
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "column {column:?} of relation {tbl:?} does not exist"
                ))
            })?;
        let col = &mut table.schema_mut().columns[pos];
        if is_runtime {
            col.runtime_default = Some(display);
            col.default = None;
        } else if let Some(v) = literal_value {
            col.default = Some(v);
            col.runtime_default = None;
        } else {
            // Could not evaluate; fall back to runtime path.
            col.runtime_default = Some(display);
            col.default = None;
        }
        Ok(())
    }

    fn alter_column_drop_default(&mut self, tbl: &str, column: String) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN DROP DEFAULT: column {column:?} not in table {tbl:?}"
                ))
            })?;
        let col = &mut table.schema_mut().columns[pos];
        col.default = None;
        col.runtime_default = None;
        Ok(())
    }

    /// v7.37.18 (18.2) — set / drop column NOT NULL flag.
    fn alter_column_set_not_null(&mut self, tbl: &str, column: String) -> Result<(), EngineError> {
        // Validate no existing row holds NULL in this column
        // before flipping the flag. PG raises on first NULL hit.
        // v7.39 (read01 round 49) — scan VISIBLE rows, not physical ones.
        // Under in-place MVCC a DELETE leaves a tombstoned physical row
        // behind; counting it made `DELETE FROM t; ALTER TABLE t ALTER c SET
        // NOT NULL` fail on a table PG sees as empty (the flip-regression
        // family: same shape as the ATTACH PARTITION empty-check and the
        // ALTER TYPE rewrite bug).
        let snap = self.current_snapshot();
        let table = self.active_catalog().get(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "column {column:?} of relation {tbl:?} does not exist"
                ))
            })?;
        for (_, row) in table.scan_visible(&snap) {
            if matches!(row.values.get(pos), Some(spg_storage::Value::Null)) {
                // v7.39 (read01 round 49) — PG wording (23502 at the wire).
                return Err(EngineError::Unsupported(alloc::format!(
                    "column {column:?} of relation {tbl:?} contains null values"
                )));
            }
        }
        let table = self
            .active_catalog_mut()
            .get_mut(tbl)
            .expect("checked above");
        table.schema_mut().columns[pos].nullable = false;
        Ok(())
    }

    fn alter_column_drop_not_null(&mut self, tbl: &str, column: String) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN DROP NOT NULL: column {column:?} not in table {tbl:?}"
                ))
            })?;
        table.schema_mut().columns[pos].nullable = true;
        Ok(())
    }

    /// v7.37.16 (16.3) — `ALTER TABLE parent ATTACH PARTITION child <bounds>`.
    ///
    /// Promotes an existing standalone table `child` into a partition
    /// of `parent`. Enforces:
    ///   1. `parent` is a partition parent (`PartitionRole::Parent`).
    ///   2. `child` is currently standalone (`partition_role == None`).
    ///   3. `child`'s column list is layout-compatible with `parent`
    ///      (same column names, types and ordering — PG also requires
    ///      this and uses it to delegate the actual storage).
    ///   4. `bounds` shape matches `parent.kind` (Range/List/Hash).
    ///   5. New range / list / hash bounds don't overlap any existing
    ///      sibling — same gates as the CREATE TABLE … PARTITION OF
    ///      path.
    ///   6. Every existing row in `child` satisfies the bound predicate
    ///      (PG's "partition constraint" check). Mis-fits raise; no
    ///      silent re-routing.
    fn alter_attach_partition(
        &mut self,
        parent_name: &str,
        child_name: String,
        bounds: spg_sql::ast::PartitionOfBoundsAst,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::PartitionOfBoundsAst;
        use spg_storage::{PartitionKind, PartitionRole};
        // Parent gate.
        let (parent_kind, parent_columns) = {
            let parent = self.active_catalog().get(parent_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: parent_name.into(),
                })
            })?;
            match &parent.schema().partition_role {
                Some(PartitionRole::Parent { kind, .. }) => {
                    (*kind, parent.schema().columns.clone())
                }
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ALTER TABLE … ATTACH PARTITION: {parent_name:?} is not a partition parent"
                    )));
                }
            }
        };
        // Child gate: must exist + be standalone + share parent's
        // column layout.
        {
            let child = self.active_catalog().get(&child_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: child_name.clone(),
                })
            })?;
            if child.schema().partition_role.is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE … ATTACH PARTITION: {child_name:?} is already a partition; \
                     DETACH it first"
                )));
            }
            let child_cols = &child.schema().columns;
            if child_cols.len() != parent_columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE … ATTACH PARTITION: column-count mismatch \
                     ({child_name:?} has {}, {parent_name:?} has {})",
                    child_cols.len(),
                    parent_columns.len()
                )));
            }
            for (c, p) in child_cols.iter().zip(parent_columns.iter()) {
                if !c.name.eq_ignore_ascii_case(&p.name) || c.ty != p.ty {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ALTER TABLE … ATTACH PARTITION: column {:?} of {child_name:?} \
                         (type {:?}) doesn't match column {:?} of {parent_name:?} (type {:?})",
                        c.name,
                        c.ty,
                        p.name,
                        p.ty
                    )));
                }
            }
        }
        // Resolve bounds (same gates as CREATE TABLE … PARTITION OF).
        let role = match bounds {
            PartitionOfBoundsAst::Default => PartitionRole::Default {
                parent_name: parent_name.into(),
            },
            PartitionOfBoundsAst::Range { lower, upper } => {
                if !matches!(parent_kind, PartitionKind::Range) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ATTACH PARTITION: FOR VALUES FROM/TO only valid for a RANGE-partitioned \
                         parent (parent {parent_name:?} is {parent_kind:?})"
                    )));
                }
                let lower_b = crate::partition::evaluate_partition_bound(*lower)?;
                let upper_b = crate::partition::evaluate_partition_bound(*upper)?;
                if !crate::partition::ranges_overlap(&lower_b, &upper_b, &lower_b, &upper_b) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ATTACH PARTITION: FROM ({}) TO ({}) is empty (lower must be < upper)",
                        crate::partition::bound_to_diag(&lower_b),
                        crate::partition::bound_to_diag(&upper_b),
                    )));
                }
                for sib in crate::partition::children_of_parent(self.active_catalog(), parent_name)
                {
                    let Some(t) = self.active_catalog().get(&sib) else {
                        continue;
                    };
                    if let Some(PartitionRole::Range {
                        lower: sl,
                        upper: su,
                        ..
                    }) = &t.schema().partition_role
                    {
                        if crate::partition::ranges_overlap(&lower_b, &upper_b, sl, su) {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "ATTACH PARTITION: range FROM ({}) TO ({}) overlaps sibling \
                                 {sib:?} (FROM ({}) TO ({}))",
                                crate::partition::bound_to_diag(&lower_b),
                                crate::partition::bound_to_diag(&upper_b),
                                crate::partition::bound_to_diag(sl),
                                crate::partition::bound_to_diag(su),
                            )));
                        }
                    }
                }
                PartitionRole::Range {
                    parent_name: parent_name.into(),
                    lower: lower_b,
                    upper: upper_b,
                }
            }
            PartitionOfBoundsAst::List { values } => {
                if !matches!(parent_kind, PartitionKind::List) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ATTACH PARTITION: FOR VALUES IN only valid for a LIST-partitioned \
                         parent (parent {parent_name:?} is {parent_kind:?})"
                    )));
                }
                let mut bounds_v = Vec::with_capacity(values.len());
                for v in values {
                    bounds_v.push(crate::partition::evaluate_partition_bound(v)?);
                }
                for sib in crate::partition::children_of_parent(self.active_catalog(), parent_name)
                {
                    let Some(t) = self.active_catalog().get(&sib) else {
                        continue;
                    };
                    if let Some(PartitionRole::List {
                        values: existing, ..
                    }) = &t.schema().partition_role
                    {
                        for new_b in &bounds_v {
                            if existing.iter().any(|e| e == new_b) {
                                // v7.39 (round 770) — PG's overlap sentence.
                                let _ = crate::partition::bound_to_diag(new_b);
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "partition \"{child_name}\" would overlap partition \"{sib}\"",
                                )));
                            }
                        }
                    }
                }
                PartitionRole::List {
                    parent_name: parent_name.into(),
                    values: bounds_v,
                }
            }
            PartitionOfBoundsAst::Hash { modulus, remainder } => {
                if !matches!(parent_kind, PartitionKind::Hash) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ATTACH PARTITION: FOR VALUES WITH only valid for a HASH-partitioned \
                         parent (parent {parent_name:?} is {parent_kind:?})"
                    )));
                }
                if modulus == 0 || remainder >= modulus {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ATTACH PARTITION: HASH (MODULUS={modulus}, REMAINDER={remainder}) \
                         must satisfy modulus > 0 and remainder < modulus"
                    )));
                }
                for sib in crate::partition::children_of_parent(self.active_catalog(), parent_name)
                {
                    let Some(t) = self.active_catalog().get(&sib) else {
                        continue;
                    };
                    if let Some(PartitionRole::Hash {
                        modulus: m,
                        remainder: r,
                        ..
                    }) = &t.schema().partition_role
                    {
                        if *m != modulus {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "ATTACH PARTITION: HASH MODULUS {modulus} differs from sibling \
                                 {sib:?} MODULUS {m} (mixed moduli not yet supported)"
                            )));
                        }
                        if *r == remainder {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "ATTACH PARTITION: HASH REMAINDER {remainder} already used \
                                 by sibling {sib:?}"
                            )));
                        }
                    }
                }
                PartitionRole::Hash {
                    parent_name: parent_name.into(),
                    modulus,
                    remainder,
                }
            }
        };
        // PG-style "partition constraint" check — every existing row
        // in child must satisfy the new role's predicate. For now we
        // leave row-validation as TODO (16.3.b): pre-existing rows
        // could violate the bound. v7.37.16.3 ships with a
        // pessimistic gate: refuse ATTACH if the child has any rows
        // and require the operator to either DROP them first or use
        // a fresh empty child. This matches PG's safest behaviour
        // (PG actually scans the rows; our scan path lands in
        // 16.3.b). Match the spirit, not the letter.
        // Count *visible* rows: under in-place MVCC a DELETE leaves a
        // tombstoned physical row behind, which must not fail the
        // empty-child gate (legacy path removed it physically).
        // v7.39 (round 621) — 16.3.b, the row scan the gate above promised.
        //
        // The pessimistic "child must be empty" gate refused the ordinary
        // migration — build a table, load it, attach it — that partitioned
        // setups are adopted FOR. PG scans the rows; now so does this. Every
        // visible row's key must satisfy the new bound, and one that does not
        // raises PG's wording (`partition constraint of relation … is violated
        // by some row`) BEFORE the role is installed, so a failed attach
        // changes nothing.
        let key_pos = {
            let parent = self.active_catalog().get(parent_name);
            match parent.and_then(|p| p.schema().partition_role.as_ref()) {
                Some(spg_storage::PartitionRole::Parent {
                    key_column_positions,
                    ..
                }) => key_column_positions.first().copied().unwrap_or(0),
                _ => 0,
            }
        };
        let snap = self.current_snapshot();
        if let Some(t) = self.active_catalog().get(&child_name) {
            for (_, row) in t.scan_visible(&snap) {
                let key = row.values.get(key_pos).cloned().unwrap_or(Value::Null);
                let fits = match &role {
                    PartitionRole::Range { lower, upper, .. } => {
                        crate::partition::value_to_bound(&key)
                            .is_some_and(|b| crate::partition::value_in_range(&b, lower, upper))
                    }
                    PartitionRole::List { values, .. } => {
                        values.iter().any(|b| b.equals_value(&key))
                    }
                    PartitionRole::Hash {
                        modulus, remainder, ..
                    } => {
                        crate::partition::pg_compatible_hash(&key).rem_euclid(u64::from(*modulus))
                            == u64::from(*remainder)
                    }
                    // A DEFAULT partition takes whatever no sibling claims, so
                    // any existing row satisfies it.
                    // v7.39 (round 645) — an inheritance child has no key
                    // constraint at all: nothing it holds can fail to fit.
                    PartitionRole::Default { .. }
                    | PartitionRole::Parent { .. }
                    | PartitionRole::Inherits { .. } => true,
                };
                if !fits {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "partition constraint of relation {child_name:?} is violated by some row"
                    )));
                }
            }
        }
        // Install role.
        let child = self
            .active_catalog_mut()
            .get_mut(&child_name)
            .expect("child existed above");
        child.schema_mut().partition_role = Some(role);
        Ok(())
    }

    /// v7.37.16 (16.4 + 16.5) — `ALTER TABLE parent DETACH PARTITION
    /// child [CONCURRENTLY] [FINALIZE]`.
    ///
    /// Demotes a partition back to a standalone table by clearing
    /// `partition_role`. CONCURRENTLY + FINALIZE are accepted at the
    /// parser; semantically SPG's single-engine model lets us detach
    /// atomically (PG's two-phase split addresses replication lag,
    /// which doesn't apply here).
    fn alter_detach_partition(
        &mut self,
        parent_name: &str,
        child_name: String,
        _concurrently: bool,
        _finalize: bool,
    ) -> Result<(), EngineError> {
        use spg_storage::PartitionRole;
        // Parent gate.
        {
            let parent = self.active_catalog().get(parent_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: parent_name.into(),
                })
            })?;
            if !matches!(
                parent.schema().partition_role,
                Some(PartitionRole::Parent { .. })
            ) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE … DETACH PARTITION: {parent_name:?} is not a partition parent"
                )));
            }
        }
        // Child gate: must be a partition of THIS parent.
        {
            let child = self.active_catalog().get(&child_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: child_name.clone(),
                })
            })?;
            let parent_of_child = match &child.schema().partition_role {
                Some(PartitionRole::Range { parent_name, .. })
                | Some(PartitionRole::List { parent_name, .. })
                | Some(PartitionRole::Hash { parent_name, .. })
                | Some(PartitionRole::Default { parent_name }) => parent_name.clone(),
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "DETACH PARTITION: {child_name:?} is not a partition"
                    )));
                }
            };
            if parent_of_child != parent_name {
                return Err(EngineError::Unsupported(alloc::format!(
                    "DETACH PARTITION: {child_name:?} is a partition of {parent_of_child:?}, \
                     not {parent_name:?}"
                )));
            }
        }
        // Clear role.
        let child = self
            .active_catalog_mut()
            .get_mut(&child_name)
            .expect("child existed above");
        child.schema_mut().partition_role = None;
        Ok(())
    }

    /// v7.39 (round 647) — `ALTER TABLE c INHERIT p` / `NO INHERIT p`.
    ///
    /// Measured on PG18: after `NO INHERIT`, the parent stops seeing the
    /// child's rows, `pg_inherits` loses the row, and the child keeps
    /// everything it had. `INHERIT` puts it back. Neither moves a row.
    ///
    /// A child of several parents keeps the others; the parent list is
    /// ordered, and dropping one from the middle leaves the rest in
    /// place — which is also what makes `pg_inherits.inhseqno` keep
    /// meaning what it means.
    fn alter_inherit(
        &mut self,
        child: &str,
        parent: &str,
        detach: bool,
    ) -> Result<(), EngineError> {
        use spg_storage::PartitionRole;
        if self.active_catalog().get(parent).is_none() {
            return Err(EngineError::Storage(
                spg_storage::StorageError::TableNotFound {
                    name: parent.to_string(),
                },
            ));
        }
        let Some(t) = self.active_catalog_mut().get_mut(child) else {
            return Err(EngineError::Storage(
                spg_storage::StorageError::TableNotFound {
                    name: child.to_string(),
                },
            ));
        };
        let current = match &t.schema().partition_role {
            Some(PartitionRole::Inherits { parent_names }) => parent_names.clone(),
            Some(_) => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{child:?} is a partition, not an inheritance child"
                )));
            }
            None => Vec::new(),
        };
        let mut names = current;
        if detach {
            let before = names.len();
            names.retain(|p| !p.eq_ignore_ascii_case(parent));
            if names.len() == before {
                // v7.39 (round 652) — PG names the PARENT first:
                // `relation "parent" is not a parent of relation "child"`.
                // SPG had the two the other way round, so a client
                // matching on the message read the wrong relation as the
                // one at fault.
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation {parent:?} is not a parent of relation {child:?}"
                )));
            }
        } else {
            if names.iter().any(|p| p.eq_ignore_ascii_case(parent)) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation {child:?} would be inherited from {parent:?} more than once"
                )));
            }
            names.push(parent.to_string());
        }
        t.schema_mut().partition_role = if names.is_empty() {
            None
        } else {
            Some(PartitionRole::Inherits {
                parent_names: names,
            })
        };
        Ok(())
    }

    fn alter_set_hot_tier_bytes(&mut self, tbl: &str, n: u64) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        table.schema_mut().hot_tier_bytes = Some(n);
        Ok(())
    }

    fn alter_add_foreign_key(
        &mut self,
        tbl: &str,
        fk: spg_sql::ast::ForeignKeyConstraint,
    ) -> Result<(), EngineError> {
        // v7.6.8 — resolve FK against the live catalog first
        // (validates parent table, columns, indices). Then
        // verify every existing row in the child table
        // satisfies the new constraint. Then install it.
        let cols_snapshot = self
            .active_catalog()
            .get(tbl)
            .ok_or_else(|| EngineError::Storage(StorageError::TableNotFound { name: tbl.into() }))?
            .schema()
            .columns
            .clone();
        let storage_fk = resolve_foreign_key(tbl, &cols_snapshot, fk, self.active_catalog())?;
        // Verify existing rows. Treat them as a virtual
        // INSERT batch — reusing the v7.6.2 enforce helper.
        let existing_rows: Vec<Vec<Value<'static>>> = self
            .active_catalog()
            .get(tbl)
            .expect("checked above")
            .rows()
            .iter()
            .map(|r| r.values.clone())
            .collect();
        enforce_fk_inserts(
            self.active_catalog(),
            tbl,
            core::slice::from_ref(&storage_fk),
            &existing_rows,
        )?;
        // Reject duplicate constraint name.
        let table = self
            .active_catalog_mut()
            .get_mut(tbl)
            .expect("checked above");
        if let Some(name) = &storage_fk.name
            && table
                .schema()
                .foreign_keys
                .iter()
                .any(|f| f.name.as_ref() == Some(name))
        {
            // v7.39 (read01 round 47) — PG wording (42710).
            return Err(EngineError::Unsupported(alloc::format!(
                "constraint {name:?} for relation {tbl:?} already exists"
            )));
        }
        table.schema_mut().foreign_keys.push(storage_fk);
        Ok(())
    }

    /// v7.13.2 / v7.37.18 (18.17 widened) — DROP CONSTRAINT for
    /// FK + PK/UNIQUE + CHECK. Originally FK-only; widened to
    /// match PG's behaviour where `ALTER TABLE t DROP CONSTRAINT
    /// t_pkey` removes a PRIMARY KEY just like it would an FK.
    fn alter_drop_foreign_key(
        &mut self,
        tbl: &str,
        name: String,
        if_exists: bool,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        // v7.39 (read01 round 48) — 0) the stored name wins. A constraint
        // created with `ADD CONSTRAINT <name> …` (or the inline `CONSTRAINT
        // <name>` form) now carries that name, so DROP finds it directly.
        // Catalogs written before FILE_VERSION 60 have no stored names and
        // fall through to the synthesised-name lookups below, which stay
        // exactly as they were.
        {
            let ucs = &mut table.schema_mut().uniqueness_constraints;
            let before = ucs.len();
            ucs.retain(|u| u.name.as_deref() != Some(name.as_str()));
            if ucs.len() != before {
                return Ok(());
            }
            let checks = &mut table.schema_mut().checks;
            let before = checks.len();
            checks.retain(|c| c.name.as_deref() != Some(name.as_str()));
            if checks.len() != before {
                return Ok(());
            }
        }
        // 1) Try foreign keys.
        let fks = &mut table.schema_mut().foreign_keys;
        let fk_before = fks.len();
        fks.retain(|f| f.name.as_ref() != Some(&name));
        if fks.len() != fk_before {
            return Ok(());
        }
        // 2) Try PK / UNIQUE constraints by their SYNTHESISED name.
        //    v7.39 (read01 round 48) — resolve through the very
        //    synthesisers pg_constraint / pg_get_constraintdef report from
        //    (`pg_unique_conname` / `pg_check_connames`), so a name the
        //    catalog shows is always a name DROP accepts. The old ad-hoc
        //    `<table>_uniqN` / `<table>_checkN` prefixes never matched what
        //    the views printed (`<table>_<col>_key` / `<table>_<col>_check`).
        // (Single-column UNIQUE indices that don't have a UC entry need to go
        // through `DROP INDEX <name>` instead — indices are a slice, not a Vec.)
        let uc_hit = table.schema().uniqueness_constraints.iter().position(|uc| {
            uc.name.is_none() && crate::system_catalog::pg_unique_conname(table, uc, tbl) == name
        });
        if let Some(idx) = uc_hit {
            table.schema_mut().uniqueness_constraints.remove(idx);
            return Ok(());
        }
        // 3) CHECK constraints by their synthesised name.
        let check_names =
            crate::system_catalog::pg_check_connames(table, tbl, &table.schema().checks);
        let check_hit = check_names.iter().position(|n| *n == name);
        if let Some(idx) = check_hit {
            let checks = &mut table.schema_mut().checks;
            if idx < checks.len() {
                checks.remove(idx);
                return Ok(());
            }
        }
        // Nothing matched; respect IF EXISTS.
        if if_exists {
            return Ok(());
        }
        // v7.39 (read01 round 47) — PG wording (42704). Note PG's own
        // inconsistency: DROP CONSTRAINT says "of relation" while ADD
        // CONSTRAINT says "for relation" — both are matched verbatim.
        Err(EngineError::Unsupported(alloc::format!(
            "constraint {name:?} of relation {tbl:?} does not exist"
        )))
    }

    fn alter_add_column(
        &mut self,
        tbl: &str,
        column: ColumnDef,
        if_not_exists: bool,
    ) -> Result<(), EngineError> {
        // v7.13.0 — mailrs round-5 G1. Append-only column add
        // with back-fill of the DEFAULT (or NULL) into every
        // existing row. Column positions don't shift, so we
        // skip index rebuild.
        let clock = self.clock;
        let add_mysql = self.backslash_escapes;
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        if table
            .schema()
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&column.name))
        {
            if if_not_exists {
                // v7.39 (read01 round 46) — PG's IF NOT EXISTS skip NOTICE.
                self.notice(alloc::format!(
                    "column {:?} of relation {:?} already exists, skipping",
                    column.name,
                    tbl
                ));
                return Ok(());
            }
            // v7.39 (read01 round 45) — PG wording (42701 at the wire).
            return Err(EngineError::Unsupported(alloc::format!(
                "column {:?} of relation {:?} already exists",
                column.name,
                tbl
            )));
        }
        let col_name = column.name.clone();
        let nullable = column.nullable;
        let has_default = column.default.is_some() || column.auto_increment;
        // v7.38.3 (sentori 2.2) — the inline `CHECK (…)` on an ADD COLUMN.
        // The parser has always put it on the ColumnDef and this path has
        // never read it, so `ALTER TABLE t ADD COLUMN env text CHECK (env
        // IN ('a','b'))` was ACCEPTED and registered nothing: pg_constraint
        // showed no row and a violating INSERT went in. A constraint that
        // silently does not exist is worse than one that loudly does not
        // work. (The separate `ADD CONSTRAINT` form was always enforced —
        // only the inline-on-ADD-COLUMN spelling vanished.)
        let inline_check = column.check.clone().map(|e| e.to_string());
        let col_schema = column_def_to_schema(column, add_mysql)?;
        let row_count = table.row_count();
        // Compute the back-fill value. Literal / runtime DEFAULT
        // funnels through the same resolver that INSERT uses
        // (v7.9.21 `resolve_column_default_free`). NULL when
        // the column is nullable and has no DEFAULT. NOT NULL
        // without DEFAULT errors when the table has existing
        // rows — same as PG.
        let fill_value: Value<'static> = if has_default || col_schema.runtime_default.is_some() {
            resolve_column_default_free(&col_schema, clock, None)?
        } else if nullable || row_count == 0 {
            Value::Null
        } else {
            // v7.39 (read01 round 89) — PG's exact wording (23502):
            // `column "req" of relation "t" contains null values`.
            return Err(EngineError::Unsupported(alloc::format!(
                "column \"{col_name}\" of relation \"{tbl}\" contains null values"
            )));
        };
        table.add_column(col_schema, fill_value);
        // The column exists before the CHECK is validated, because the
        // predicate is written in terms of it. PG validates against the
        // rows already there and refuses the whole statement if any fails
        // — measured: adding `e text CHECK (e IS NOT NULL)` to a table
        // with a row errors ("is violated by some row"), while the same
        // column with a DEFAULT that satisfies it succeeds. On refusal the
        // column has to come back out; nothing else has happened yet.
        if let Some(src) = inline_check {
            let pos = table.schema().columns.len() - 1;
            let name = alloc::format!("{tbl}_{col_name}_check");
            if let Err(e) =
                crate::constraints::validate_check_against_existing_rows(table, tbl, &name, &src)
            {
                table.drop_column(pos);
                return Err(e);
            }
            table
                .schema_mut()
                .checks
                .push(spg_storage::CheckConstraint {
                    // Unnamed: `pg_check_connames` synthesises PG's
                    // `<table>_<column>_check` from the referenced column, the
                    // same name the CREATE TABLE spelling gets.
                    name: None,
                    expr: src,
                    validated: true,
                });
        }
        Ok(())
    }

    fn alter_column_type(
        &mut self,
        tbl: &str,
        column: String,
        new_type: spg_sql::ast::ColumnTypeName,
        using: Option<Expr>,
        collation: Option<(spg_sql::ast::Collation, alloc::string::String)>,
    ) -> Result<(), EngineError> {
        // v7.13.0 — mailrs round-5 G8. Re-evaluate each
        // row's column value (either through the USING
        // expression if supplied, or as a direct CAST of
        // the existing value) and re-coerce to the new
        // type. Indices on the column get rebuilt.
        let new_data_type = column_type_to_data_type(new_type);
        // v7.39 (round 713) — `TYPE <ty> COLLATE <name>`. PG refuses a
        // collation on a non-collatable type; on a collatable one it
        // re-collates, and NO clause resets to the type default (both
        // measured round 713). The clause parsed here all along and was
        // dropped — the statement succeeded, the ordering never changed.
        let is_collatable = matches!(
            new_data_type,
            DataType::Text | DataType::Varchar(_) | DataType::Char(_)
        );
        if collation.is_some() && !is_collatable {
            let spelled = crate::conversions::regtype_oid_to_name(
                crate::system_catalog::pg_type_oid(new_data_type),
            )
            .unwrap_or("this type");
            return Err(EngineError::Unsupported(alloc::format!(
                "collations are not supported by type {spelled}"
            )));
        }
        // v7.38.18 (G2) — a collation PostgreSQL does not have is not a
        // collation, and PG 18.4 says so: `collation "x" for encoding
        // "UTF8" does not exist`. Round 670 chose warn-not-refuse under
        // the zero-customer-change ruling, when this build could perform
        // almost nothing and refusing would have failed working DDL.
        // That calculus has inverted: 880 names are performable now, so
        // the only ones refused here are the ones PG refuses too, and
        // refusing is what keeps a customer's DDL behaving the same.
        //
        // The dialect decides, because MySQL's names are not in PG's
        // catalogue and PG rejects them — measured on 18.4.
        if let Some((_, name)) = &collation
            && !crate::collate::is_known(name)
        {
            return Err(crate::collate::unknown_collation_error(name));
        }
        // v7.38.18 — the warning that used to stand here said range
        // comparisons "still compare by bytes". That stopped being true
        // in this version: a declared collation reaches `<`, `BETWEEN`
        // and the index keys, verified against PG 18.4. A warning that
        // is false is worse than none, so only the unperformable case
        // keeps one.
        if let Some((_, name)) = &collation
            && !crate::collate::is_supported(name)
        {
            self.warning(alloc::format!(
                "column \"{column}\" declares COLLATE \"{name}\", which this build \
                 cannot perform; SPG records the declaration and orders this column \
                 by bytes (the C collation)"
            ));
        }
        let mysql_dialect = self.backslash_escapes;
        // v7.39 — under in-place MVCC the row store carries tombstoned
        // versions; their dead values must not join the rewrite (an
        // INT corpse under a TEXT conversion would abort the whole
        // ALTER). Snapshot BEFORE the &mut borrow.
        let scan_snapshot = self.current_snapshot();
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let col_pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "column {column:?} of relation {:?} does not exist",
                    tbl
                ))
            })?;
        // v7.36 (cold-tier coverage) — ALTER COLUMN TYPE rewrites
        // every row's value to the new representation. Cold-tier
        // rows live in segments encoded against the OLD type and
        // can't be rewritten in-place from this path; doing the
        // ALTER anyway would leave the segments unreadable under
        // the new schema. Match PG / MariaDB's invariant of "never
        // half-apply a schema change" by raising explicitly.
        // v7.39 (round 456) — O(1) predicate first; see the DELETE path.
        if table.has_cold_rows_fast() && table.count_cold_locators() > 0 {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER COLUMN TYPE on {tbl:?}: cold-tier rows exist for this table; \
                 cold-tier schema rewrite is a v7.37 candidate. Run COMPACT to bring \
                 the cold rows back to the hot tier and retry."
            )));
        }
        let schema_cols = table.schema().columns.clone();
        let ctx = eval::EvalContext::new(&schema_cols, None);
        // `None` = a tombstoned version: left untouched entirely (its
        // slot is never rewritten, so the update_row type check on the
        // NEW schema never sees the old-type corpse).
        let mut new_values: alloc::vec::Vec<Option<Value<'static>>> =
            alloc::vec::Vec::with_capacity(table.row_count());
        for (ri, row) in table.rows().iter().enumerate() {
            if !table.is_row_visible(ri, &scan_snapshot) {
                new_values.push(None);
                continue;
            }
            let raw = match &using {
                Some(expr) => eval::eval_expr(expr, row, &ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "ALTER COLUMN TYPE: USING expression failed: {e:?}"
                    ))
                })?,
                None => row.values.get(col_pos).cloned().unwrap_or(Value::Null),
            };
            // v7.39 — PG's ALTER TYPE without USING applies the
            // assignment cast, which is wider than INSERT's strict
            // coercion: any value casts to the text family through
            // its output function (INT -> TEXT rewrites the column),
            // while a narrowing like TEXT -> INT is refused with
            // PG's phrasing + HINT. A USING expression bypasses this
            // (its result must strictly coerce).
            let coerced = match coerce_value(raw.clone(), new_data_type, &column, col_pos) {
                Ok(v) => v,
                Err(_)
                    if using.is_none()
                        && matches!(
                            new_data_type,
                            DataType::Text | DataType::Varchar(_) | DataType::Char(_)
                        ) =>
                {
                    coerce_value(
                        Value::text(crate::eval::value_to_text(&raw)),
                        new_data_type,
                        &column,
                        col_pos,
                    )?
                }
                Err(e) => {
                    if using.is_none() {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{column}\" cannot be cast automatically to type \
                             {new_data_type:?}; You might need to specify a USING expression"
                        )));
                    }
                    return Err(e);
                }
            };
            new_values.push(Some(coerced));
        }
        table.schema_mut().columns[col_pos].ty = new_data_type;
        // v7.39 (round 713) — the collation lands with the type, exactly
        // as CREATE TABLE lands it (the round-370/676 pair of fields).
        // An absent clause is a RESET, not a keep: PG re-derives the
        // collation from the new type, so `TYPE text` alone takes the
        // column back to the default — under the MySQL dialect that
        // default is the folding collation, everywhere else byte order.
        {
            let sc = &mut table.schema_mut().columns[col_pos];
            match &collation {
                Some((cenum, name)) => {
                    sc.collation_name = Some(name.clone());
                    sc.collation = match cenum {
                        spg_sql::ast::Collation::Binary => spg_storage::Collation::Binary,
                        spg_sql::ast::Collation::CaseInsensitive => {
                            spg_storage::Collation::CaseInsensitive
                        }
                    };
                }
                None => {
                    sc.collation_name = None;
                    sc.collation = if mysql_dialect && is_collatable {
                        spg_storage::Collation::CaseInsensitive
                    } else {
                        spg_storage::Collation::Binary
                    };
                }
            }
        }
        for (i, v) in new_values.into_iter().enumerate() {
            let Some(v) = v else { continue };
            let mut row_values = table
                .rows()
                .get(i)
                .expect("bounds-checked above")
                .values
                .clone();
            row_values[col_pos] = v;
            table.update_row(i, row_values)?;
        }
        Ok(())
    }

    /// v7.39 (round 652) — `ALTER TABLE … VALIDATE CONSTRAINT <name>`.
    /// Scans the rows against a CHECK added `NOT VALID`; on success the
    /// constraint becomes validated and `pg_constraint.convalidated`
    /// flips, which is what makes the next pg_dump stop emitting the
    /// `NOT VALID` suffix. Validating an already-valid constraint is a
    /// no-op, as in PG.
    fn alter_validate_constraint(&mut self, tbl: &str, name: &str) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let names = crate::system_catalog::pg_check_connames(table, tbl, &table.schema().checks);
        let Some(idx) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) else {
            // PG names the relation it looked in. A constraint that is
            // not a CHECK lands here too — SPG has no unvalidated shape
            // for the others, so there is nothing this could validate.
            return Err(EngineError::Unsupported(alloc::format!(
                "constraint \"{name}\" of relation \"{tbl}\" does not exist"
            )));
        };
        if table.schema().checks[idx].validated {
            return Ok(());
        }
        let src = table.schema().checks[idx].expr.clone();
        crate::constraints::validate_check_against_existing_rows(table, tbl, name, &src)?;
        table.schema_mut().checks[idx].validated = true;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn alter_add_table_constraint(
        &mut self,
        tbl: &str,
        tc: spg_sql::ast::TableConstraint,
    ) -> Result<(), EngineError> {
        // v7.14.0 — pg_dump emits PKs as a separate
        // ALTER TABLE ADD CONSTRAINT post-CREATE-TABLE.
        // For PRIMARY KEY / UNIQUE, install a UC entry
        // and the implicit BTree index on the leading
        // column. CHECK: append predicate to schema.
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let is_pk = matches!(tc, spg_sql::ast::TableConstraint::PrimaryKey { .. });
        // v7.39 (read01 round 48) — a constraint name must be unique on the
        // table. PG rejects a re-used name with 42710; SPG used to drop the
        // name on the floor entirely, so the collision was invisible.
        let con_name: Option<String> = match &tc {
            spg_sql::ast::TableConstraint::PrimaryKey { name, .. }
            | spg_sql::ast::TableConstraint::Unique { name, .. }
            | spg_sql::ast::TableConstraint::Check { name, .. } => name.clone(),
            _ => None,
        };
        if let Some(n) = &con_name
            && constraint_name_taken(table, n)
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "constraint {n:?} for relation {tbl:?} already exists"
            )));
        }
        // v7.39 (read01 round 45) — a table may have at most one PRIMARY
        // KEY. PG rejects a second one (even on the same column) with
        // 42P16; SPG used to install it silently. SPG's own dumps emit PK
        // inline, so restore never reaches this ALTER path.
        if is_pk
            && table
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|u| u.is_primary_key)
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "multiple primary keys for table {tbl:?} are not allowed"
            )));
        }
        // v7.22 (mailrs round-13 gap 6) — carry the parsed
        // NULLS NOT DISTINCT flag through the ALTER path;
        // it was hardcoded false here while the CREATE
        // TABLE path honoured it since v7.13.
        let nnd = matches!(
            tc,
            spg_sql::ast::TableConstraint::Unique {
                nulls_not_distinct: true,
                ..
            }
        );
        // v7.39 (round 711) — carry the timing through the ALTER path too.
        let timing = match tc {
            spg_sql::ast::TableConstraint::PrimaryKey {
                deferrable,
                initially_deferred,
                ..
            }
            | spg_sql::ast::TableConstraint::Unique {
                deferrable,
                initially_deferred,
                ..
            } => (deferrable, initially_deferred),
            _ => (false, false),
        };
        match tc {
            spg_sql::ast::TableConstraint::PrimaryKey { columns, .. }
            | spg_sql::ast::TableConstraint::Unique { columns, .. } => {
                let positions: Vec<usize> = columns
                    .iter()
                    .map(|c| {
                        table
                            .schema()
                            .columns
                            .iter()
                            .position(|sc| sc.name.eq_ignore_ascii_case(c))
                            .ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "ALTER TABLE ADD CONSTRAINT: column {c:?} not found on {:?}",
                                    tbl
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // Skip if an equivalent UC is already there
                // (idempotent — pg_dump's PK + a prior inline
                // PK shouldn't double-install).
                let already = table
                    .schema()
                    .uniqueness_constraints
                    .iter()
                    .any(|u| u.columns == positions);
                if !already {
                    table.schema_mut().uniqueness_constraints.push(
                        spg_storage::UniquenessConstraint {
                            is_primary_key: is_pk,
                            columns: positions.clone(),
                            nulls_not_distinct: nnd,
                            name: con_name.clone(),
                            deferrable: timing.0,
                            initially_deferred: timing.1,
                        },
                    );
                    // PK implies NOT NULL on referenced cols.
                    if is_pk {
                        for p in &positions {
                            if let Some(c) = table.schema_mut().columns.get_mut(*p) {
                                c.nullable = false;
                            }
                        }
                    }
                    // Add a BTree index on the leading
                    // column for INSERT-side enforcement.
                    let leading = &columns[0];
                    let already_idx = table.indices().iter().any(|idx| {
                        matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                            && table.schema().columns[idx.column_position].name == *leading
                    });
                    if !already_idx {
                        let suffix = if is_pk { "pkey" } else { "key" };
                        let idx_name = alloc::format!("{}_{leading}_{suffix}", tbl);
                        let _ = table.add_index(idx_name, leading);
                    }
                }
            }
            spg_sql::ast::TableConstraint::Check {
                expr, not_valid, ..
            } => {
                let src = alloc::format!("{expr}");
                // v7.39 (round 652) — PG scans the rows already in the
                // table unless the user wrote NOT VALID, and refuses the
                // whole ALTER if any of them violates the predicate. SPG
                // used to skip that scan unconditionally, so it accepted
                // constraints PG rejects and left the table holding rows
                // that contradict its own declared CHECK — with every
                // reader, pg_dump included, believing otherwise.
                if !not_valid {
                    // The name PG puts in the message is the one the
                    // constraint would end up with, dedup suffix included,
                    // so ask for the whole prospective list and take the
                    // entry the new one occupies.
                    let mut prospective = table.schema().checks.clone();
                    prospective.push(spg_storage::CheckConstraint {
                        name: con_name.clone(),
                        expr: src.clone(),
                        validated: true,
                    });
                    let conname =
                        crate::system_catalog::pg_check_connames(table, tbl, &prospective)
                            .pop()
                            .unwrap_or_else(|| alloc::format!("{tbl}_check"));
                    crate::constraints::validate_check_against_existing_rows(
                        table, tbl, &conname, &src,
                    )?;
                }
                table
                    .schema_mut()
                    .checks
                    .push(spg_storage::CheckConstraint {
                        name: con_name.clone(),
                        expr: src,
                        validated: !not_valid,
                    });
            }
            spg_sql::ast::TableConstraint::Index { name, columns } => {
                // v7.15.0 — ALTER TABLE ADD KEY (cols).
                // mysqldump occasionally emits this
                // post-CREATE-TABLE shape; build a BTree
                // on the leading column using the
                // user-supplied or synthesised name.
                //
                // v7.39 (round 431) — the outcome now matches a measured
                // MariaDB 11 run in three ways it did not before:
                //   * a second index on an already-indexed column is
                //     BUILT, not skipped. Skipping it made the following
                //     `DROP INDEX <that name>` fail with "does not
                //     exist" — the name was never registered.
                //   * a name collision raises 42710 (MariaDB: 1061
                //     "Duplicate key name") instead of being swallowed.
                //   * an unknown column raises 42703 (MariaDB: 1072 "Key
                //     column doesn't exist in table") instead of being
                //     swallowed into a no-op.
                let leading = &columns[0];
                let idx_name = match name {
                    Some(n) => n.clone(),
                    // Unnamed `ADD INDEX (col)` takes the column's own
                    // name, with `_2`, `_3`, … on collision — measured
                    // on MariaDB 11.
                    None => {
                        let mut candidate = leading.clone();
                        let mut n = 1;
                        while table.indices().iter().any(|idx| idx.name == candidate) {
                            n += 1;
                            candidate = alloc::format!("{leading}_{n}");
                        }
                        candidate
                    }
                };
                table
                    .add_index(idx_name, leading)
                    .map_err(EngineError::Storage)?;
            }
            spg_sql::ast::TableConstraint::FulltextIndex { name, columns } => {
                // v7.17.0 Phase 2.2 — ALTER TABLE ADD
                // FULLTEXT KEY (cols). Builds one
                // fulltext-GIN per named column so MATCH
                // AGAINST gets a real inverted index.
                // Multi-column declarations expand to
                // per-column GINs (the leading column
                // drives MATCH AGAINST planning).
                for (k, col) in columns.iter().enumerate() {
                    let already_idx = table.indices().iter().any(|idx| {
                        matches!(idx.kind, spg_storage::IndexKind::GinFulltext(_))
                            && table.schema().columns[idx.column_position].name == *col
                    });
                    if already_idx {
                        continue;
                    }
                    let idx_name = match (&name, columns.len(), k) {
                        (Some(n), 1, _) => n.clone(),
                        (Some(n), _, k) => alloc::format!("{n}_{k}"),
                        (None, _, _) => {
                            alloc::format!("{}_{col}_ftidx", tbl)
                        }
                    };
                    let _ = table.add_gin_fulltext_index(idx_name, col);
                }
            }
            spg_sql::ast::TableConstraint::Exclude {
                name,
                method,
                elements,
            } => {
                // v7.39 (round 210/211) — ALTER TABLE ADD EXCLUDE. Resolve
                // element columns to positions and synthesise PG's
                // `<table>_<col…>_excl` name (ALL element columns joined by
                // `_`, e.g. `book_room_during_excl`) when unnamed.
                let mut els = Vec::with_capacity(elements.len());
                let cols_joined = elements
                    .iter()
                    .map(|(c, _)| c.clone())
                    .collect::<Vec<_>>()
                    .join("_");
                for (col, op) in elements {
                    let pos = table
                        .schema()
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&col))
                        .ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "ALTER TABLE ADD EXCLUDE: column {col:?} not found on {tbl:?}"
                            ))
                        })?;
                    els.push((pos, op));
                }
                let ex_name = name.unwrap_or_else(|| alloc::format!("{tbl}_{cols_joined}_excl"));
                table
                    .schema_mut()
                    .exclusion_constraints
                    .push(spg_storage::ExclusionConstraint {
                        name: ex_name,
                        method,
                        elements: els,
                    });
            }
        }
        Ok(())
    }

    fn alter_drop_column(
        &mut self,
        tbl: &str,
        column: String,
        if_exists: bool,
        cascade: bool,
    ) -> Result<(), EngineError> {
        // v7.13.3 — mailrs round-7 S8. Remove the column +
        // every row's value at that position; drop any index
        // on the column. RESTRICT (default) rejects when an
        // FK on this table or partial-index predicate
        // references the column; CASCADE removes those
        // dependents first.
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let col_pos = match table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
        {
            Some(p) => p,
            None => {
                if if_exists {
                    // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                    self.notice(alloc::format!(
                        "column {column:?} of relation {:?} does not exist, skipping",
                        tbl
                    ));
                    return Ok(());
                }
                // v7.39 (read01 round 45) — PG wording (42703 at the wire).
                return Err(EngineError::Unsupported(alloc::format!(
                    "column {column:?} of relation {:?} does not exist",
                    tbl
                )));
            }
        };
        // Dependent check: FKs whose local columns include
        // col_pos. CASCADE drops them; otherwise reject.
        let dependent_fks: Vec<usize> = table
            .schema()
            .foreign_keys
            .iter()
            .enumerate()
            .filter_map(|(i, fk)| {
                if fk.local_columns.contains(&col_pos) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if !dependent_fks.is_empty() && !cascade {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE DROP COLUMN {column:?}: column has FK dependents; \
                         use DROP COLUMN ... CASCADE to remove them"
            )));
        }
        // CASCADE the FK removals first.
        if cascade {
            // Drop in reverse so indices stay valid.
            let mut sorted = dependent_fks.clone();
            sorted.sort();
            sorted.reverse();
            let fks = &mut table.schema_mut().foreign_keys;
            for i in sorted {
                fks.remove(i);
            }
        }
        // v7.38.2 (sentori report 5) — PG's ALTER TABLE rule: "Indexes
        // and table constraints involving the column will be
        // automatically dropped as well." A CHECK left behind after its
        // column made the table permanently un-insertable (every later
        // INSERT hit ColumnNotFound on the ghost column). Any CHECK
        // whose expression references the dropped column goes with it;
        // an expression we can't parse can't be evaluated either way,
        // so it is kept untouched.
        let dropped = table.schema().columns[col_pos].name.clone();
        table.schema_mut().checks.retain(|chk| {
            let Ok(expr) = spg_sql::parser::parse_expression(&chk.expr) else {
                return true;
            };
            let mut involves = false;
            crate::visit_expr_columns_and_subqueries(
                &expr,
                &mut |c: &spg_sql::ast::ColumnName| {
                    if c.name.eq_ignore_ascii_case(&dropped) {
                        involves = true;
                    }
                },
                &mut |_| {},
            );
            !involves
        });
        // Drop the column. New helper on Table does the
        // row + schema + index shift atomically.
        table.drop_column(col_pos);
        Ok(())
    }

    fn alter_set_trigger_enabled(
        &mut self,
        tbl: &str,
        which: spg_sql::ast::TriggerSelector,
        enabled: bool,
    ) -> Result<(), EngineError> {
        // v7.16.1 — mailrs round-9 A.2.b. pg_dump
        // --disable-triggers wraps each table's data
        // block with `ALTER TABLE … DISABLE TRIGGER ALL`
        // / `… ENABLE TRIGGER ALL`. Toggle the enabled
        // flag on every matching trigger so the row-
        // write paths skip them; the catalog snapshot
        // persists the new state across restarts.
        let table_name = tbl.to_string();
        let trigs = self.active_catalog_mut().triggers_mut();
        let mut touched = false;
        for t in trigs.iter_mut() {
            if !t.table.eq_ignore_ascii_case(&table_name) {
                continue;
            }
            match &which {
                spg_sql::ast::TriggerSelector::All => {
                    t.enabled = enabled;
                    touched = true;
                }
                spg_sql::ast::TriggerSelector::Named(name) => {
                    if t.name.eq_ignore_ascii_case(name) {
                        t.enabled = enabled;
                        touched = true;
                    }
                }
            }
        }
        // PG semantics: `ALL` on a table with no
        // triggers is a no-op (no error). A `Named`
        // form pointing at a non-existent trigger
        // raises in PG; v7.16.1 also raises so we
        // don't silently lose state.
        if !touched {
            if let spg_sql::ast::TriggerSelector::Named(name) = &which {
                return Err(EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE {table_name:?} {} TRIGGER {name:?}: no such trigger on table",
                    if enabled { "ENABLE" } else { "DISABLE" },
                )));
            }
        }
        Ok(())
    }

    fn alter_set_column_auto_increment(
        &mut self,
        tbl: &str,
        column: String,
        seq_name: Option<String>,
    ) -> Result<(), EngineError> {
        // pg_dump's identity form names an IMPLICIT sequence
        // (`… AS IDENTITY ( SEQUENCE NAME s … )`) that never
        // gets its own CREATE SEQUENCE statement, while the
        // data section still calls `setval(s, …)`. Make the
        // sequence exist (idempotent) so those calls land.
        if let Some(seq) = seq_name {
            let _ = self.exec_create_sequence(spg_sql::ast::CreateSequenceStatement {
                name: seq,
                if_not_exists: true,
                temporary: false,
                data_type: None,
                options: spg_sql::ast::SequenceOptions::default(),
            })?;
        }
        // v7.22 (round-13 T2) — pg_dump's serial/identity
        // spellings (`SET DEFAULT nextval(…)` / `ADD
        // GENERATED … AS IDENTITY`) lower here: flip the
        // column's auto-increment flag so post-import
        // INSERTs without an explicit value keep numbering
        // (max+1 semantics; the dump's setval() calls are
        // no-ops by construction).
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ALTER COLUMN {column:?}: no such column on {:?}",
                    tbl
                ))
            })?;
        let col = &table.schema().columns[pos];
        if !matches!(
            col.ty,
            spg_storage::DataType::SmallInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::BigInt
        ) {
            return Err(EngineError::Unsupported(alloc::format!(
                "auto-increment applies to integer columns only ({column:?} is {:?})",
                col.ty
            )));
        }
        table.schema_mut().columns[pos].auto_increment = true;
        Ok(())
    }

    /// v7.39 (read01 round 48) — `ALTER TABLE t RENAME CONSTRAINT old TO new`.
    /// Only constraints that carry a stored name can be renamed: an unnamed
    /// one has no name to change, and its synthesised `pg_constraint` name
    /// is derived, not stored. PG's wording here says "for table" (while
    /// DROP CONSTRAINT says "of relation") — matched verbatim.
    /// v7.39 (read01 round 50) — `COMMENT ON <kind> <name> IS { 'text' | NULL }`.
    /// The object must exist (PG errors otherwise); `IS NULL` removes the
    /// comment. Stored in the catalog's comment map under `"<kind>:<name>"`
    /// and read back by obj_description / col_description / pg_description.
    pub(crate) fn exec_comment_on(
        &mut self,
        kind: &str,
        name: &str,
        comment: Option<&str>,
    ) -> Result<QueryResult, EngineError> {
        let cat = self.active_catalog();
        // Validate existence for the kinds SPG catalogues. PG's wording for a
        // missing relation is "relation \"x\" does not exist" (42P01).
        match kind {
            "table" | "view" => {
                if cat.get(name).is_none() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation {name:?} does not exist"
                    )));
                }
            }
            "column" => {
                let (tbl, col) = name.split_once('.').ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!("column {name:?} does not exist"))
                })?;
                let t = cat.get(tbl).ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!("relation {tbl:?} does not exist"))
                })?;
                if !t
                    .schema()
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(col))
                {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "column {col:?} of relation {tbl:?} does not exist"
                    )));
                }
            }
            "index" => {
                let found = cat.table_names().iter().any(|tn| {
                    cat.get(tn)
                        .is_some_and(|t| t.indices().iter().any(|i| i.name == name))
                });
                if !found {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation {name:?} does not exist"
                    )));
                }
            }
            "sequence" => {
                if !cat.has_sequence(name) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation {name:?} does not exist"
                    )));
                }
            }
            // schema / type / database / function: accepted and stored without
            // a catalogue lookup (SPG's registries for these are partial).
            _ => {}
        }
        let key = alloc::format!("{kind}:{name}");
        self.active_catalog_mut().set_comment(&key, comment);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    fn alter_rename_constraint(
        &mut self,
        tbl: &str,
        old: &str,
        new: String,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        if !constraint_name_taken(table, old) {
            return Err(EngineError::Unsupported(alloc::format!(
                "constraint {old:?} for table {tbl:?} does not exist"
            )));
        }
        if constraint_name_taken(table, &new) {
            return Err(EngineError::Unsupported(alloc::format!(
                "constraint {new:?} for relation {tbl:?} already exists"
            )));
        }
        let sch = table.schema_mut();
        for f in &mut sch.foreign_keys {
            if f.name.as_deref() == Some(old) {
                f.name = Some(new);
                return Ok(());
            }
        }
        for u in &mut sch.uniqueness_constraints {
            if u.name.as_deref() == Some(old) {
                u.name = Some(new);
                return Ok(());
            }
        }
        for c in &mut sch.checks {
            if c.name.as_deref() == Some(old) {
                c.name = Some(new);
                return Ok(());
            }
        }
        Ok(())
    }

    fn alter_rename_table(&mut self, tbl: &str, new: String) -> Result<(), EngineError> {
        // v7.16.2 — table-level rename (mailrs round-10
        // A.5 — used by migrate-042's `ALTER TABLE
        // contacts RENAME TO email_contacts`). Storage
        // helper updates the schema + by_name index +
        // dangling FK / trigger references in one
        // atomic step.
        let old = tbl.to_string();
        // v7.39 (read01 round 47) — PG rejects a rename onto a name that
        // already names a relation (42P07), including a rename onto the
        // table's own name. SPG used to accept both silently.
        if self.active_catalog().get(&new).is_some() {
            return Err(EngineError::Unsupported(alloc::format!(
                "relation {new:?} already exists"
            )));
        }
        self.active_catalog_mut()
            .rename_table(&old, &new)
            .map_err(EngineError::Storage)?;
        // r192 — carry the non-transactional DML counters to the new
        // name (PG keeps stats across a rename). After the storage
        // rename succeeded, so a failed rename leaves them keyed as-is.
        if let Some(stats) = self.table_write_stats.remove(&old) {
            self.table_write_stats.insert(new.clone(), stats);
        }
        Ok(())
    }

    fn alter_rename_column(
        &mut self,
        tbl: &str,
        old: String,
        new: String,
    ) -> Result<(), EngineError> {
        // v7.15.0 — `ALTER TABLE t RENAME [COLUMN] old TO
        // new`. Rename the column in the schema; rewrite
        // every stored source string on this table that
        // references it as a (potentially-qualified)
        // column identifier: CHECK predicates, partial-
        // index predicates, runtime DEFAULT expressions.
        // Then walk catalog triggers on this table and
        // patch any `UPDATE OF` column list. Function and
        // trigger bodies are NOT auto-rewritten — that
        // surface is dynamic SQL territory; users update
        // those separately (matches PG plpgsql behavior:
        // a column rename invalidates name-referencing
        // plpgsql at call time, not rename time).
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let col_pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&old))
            .ok_or_else(|| {
                // v7.39 (read01 round 47) — PG wording (42703). PG omits
                // the "of relation" qualifier on RENAME COLUMN (unlike the
                // ALTER COLUMN family below) — match it exactly.
                EngineError::Unsupported(alloc::format!("column {old:?} does not exist"))
            })?;
        // Reject same-name (case-insensitive) collision.
        if table
            .schema()
            .columns
            .iter()
            .enumerate()
            .any(|(i, c)| i != col_pos && c.name.eq_ignore_ascii_case(&new))
        {
            // v7.39 (read01 round 47) — PG wording (42701).
            return Err(EngineError::Unsupported(alloc::format!(
                "column {new:?} of relation {:?} already exists",
                tbl
            )));
        }
        // Schema rename first — even idempotent same-name
        // rename (`ALTER TABLE t RENAME a TO a`) needs to
        // be a no-op, not an error.
        if old.eq_ignore_ascii_case(&new) {
            return Ok(());
        }
        table.rename_column(col_pos, &new);
        // Rewrite per-column runtime_default sources on
        // every column of this table — a DEFAULT expression
        // on column X may reference column Y by name (rare,
        // but legal in PG when the value is supplied via a
        // function that takes the row).
        let n_cols = table.schema().columns.len();
        for i in 0..n_cols {
            let rt = table.schema().columns[i].runtime_default.clone();
            if let Some(src) = rt {
                let rewritten = rewrite_column_in_source(&src, &old, &new)?;
                table.schema_mut().columns[i].runtime_default = Some(rewritten);
            }
        }
        // Rewrite table-level CHECK predicates.
        let checks = table.schema().checks.clone();
        let mut new_checks = Vec::with_capacity(checks.len());
        for chk in checks {
            // v7.39 (read01 round 48) — rewrite the predicate, keep the name.
            new_checks.push(spg_storage::CheckConstraint {
                name: chk.name,
                expr: rewrite_column_in_source(&chk.expr, &old, &new)?,
                // Renaming a column does not re-scan the rows, so it cannot
                // turn an unvalidated constraint into a valid one.
                validated: chk.validated,
            });
        }
        table.schema_mut().checks = new_checks;
        // Rewrite per-index partial_predicate sources.
        let n_idx = table.indices().len();
        for i in 0..n_idx {
            let pred = table.indices()[i].partial_predicate.clone();
            if let Some(src) = pred {
                let rewritten = rewrite_column_in_source(&src, &old, &new)?;
                // SAFETY: indices_mut would be cleanest, but
                // partial_predicate is the only mutable field
                // here; reach in via the public mut accessor.
                table.set_partial_predicate(i, Some(rewritten));
            }
        }
        // Walk catalog triggers; patch `update_columns` on
        // triggers attached to this table.
        let table_name = tbl.to_string();
        for trig in self.active_catalog_mut().triggers_mut() {
            if !trig.table.eq_ignore_ascii_case(&table_name) {
                continue;
            }
            for c in &mut trig.update_columns {
                if c.eq_ignore_ascii_case(&old) {
                    *c = new.clone();
                }
            }
        }
        Ok(())
    }

    /// v6.0.4 — synchronous `ALTER INDEX <name> REBUILD [WITH
    /// (encoding = …)]`. Walks every table in the active catalog
    /// looking for an index matching `stmt.name`, then delegates the
    /// rebuild (including any encoding switch) to
    /// `Table::rebuild_nsw_index`. The "live" non-blocking
    /// optimisation is v6.0.4.1 / v6.1.x territory.
    pub(crate) fn exec_alter_index(
        &mut self,
        stmt: spg_sql::ast::AlterIndexStatement,
    ) -> Result<QueryResult, EngineError> {
        // Translate the optional SQL-side encoding choice into the
        // storage-side enum; the same SqlVecEncoding -> VecEncoding
        // bridge `column_type_to_data_type` uses.
        let spg_sql::ast::AlterIndexStatement {
            name: idx_name,
            target,
        } = stmt;
        // v7.16.2 — RENAME TO branch (mailrs round-10 migrate-042).
        // IF EXISTS makes a missing index a no-op rather than an
        // error, mirroring PG semantics.
        if let spg_sql::ast::AlterIndexTarget::Rename { new, if_exists } = target {
            let renamed = self.active_catalog_mut().rename_index(&idx_name, &new);
            return match renamed {
                Ok(()) => Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: self.catalog_change_is_committed(),
                }),
                Err(StorageError::IndexNotFound { .. }) if if_exists => {
                    Ok(QueryResult::CommandOk {
                        affected: 0,
                        modified_catalog: false,
                    })
                }
                // v7.39 (round 700) — PG18 answers `relation "x" does not
                // exist` here, not `index "x" …`. An index IS a relation
                // there, and the wire classifier reads the relation wording
                // for 42P01; SPG's own spelling missed both.
                Err(StorageError::IndexNotFound { .. }) => Err(EngineError::Unsupported(
                    alloc::format!("relation \"{idx_name}\" does not exist"),
                )),
                Err(e) => Err(EngineError::Storage(e)),
            };
        }
        // v7.39 (round 710) — SET/RESET storage params: validate the
        // index, no-op the parameters (PG resolves the relation first —
        // `relation "x" does not exist` — and SPG engine-manages storage
        // parameters, as the ALTER TABLE arms already record).
        if matches!(target, spg_sql::ast::AlterIndexTarget::StorageParams) {
            let cat = self.active_catalog();
            let exists = cat.table_names().iter().any(|tn| {
                cat.get(tn.as_str())
                    .is_some_and(|t| t.indices().iter().any(|i| i.name == idx_name))
            });
            if !exists {
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation \"{idx_name}\" does not exist"
                )));
            }
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        let spg_sql::ast::AlterIndexTarget::Rebuild { encoding } = target else {
            unreachable!("Rename branch returned above");
        };
        let target = encoding.map(|e| match e {
            SqlVecEncoding::F32 => VecEncoding::F32,
            SqlVecEncoding::Sq8 => VecEncoding::Sq8,
            SqlVecEncoding::F16 => VecEncoding::F16,
        });
        // Linear scan: index names are globally unique within a
        // catalog (enforced by add_nsw_index_inner) so the first
        // match is the only one. Save the table name to avoid
        // borrowing while we then take a mut borrow.
        let table_name = {
            let cat = self.active_catalog();
            let mut found: Option<String> = None;
            for tname in cat.table_names() {
                if let Some(t) = cat.get(&tname)
                    && t.indices().iter().any(|i| i.name == idx_name)
                {
                    found = Some(tname);
                    break;
                }
            }
            found.ok_or_else(|| {
                EngineError::Storage(StorageError::IndexNotFound {
                    name: idx_name.clone(),
                })
            })?
        };
        let table = self
            .active_catalog_mut()
            .get_mut(&table_name)
            .expect("table found above");
        table.rebuild_nsw_index(&idx_name, target)?;
        // v6.3.1 — ALTER INDEX REBUILD potentially with new encoding
        // changes cost characteristics; evict any cached plans.
        self.plan_cache.evict_referencing(&table_name);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.39 (read01 round 93) — derive PG's generated index name for an
    /// unnamed `CREATE INDEX`. PG's `ChooseIndexName` builds
    /// `<table>_<label1>_<label2>…_idx`, where each label is a key
    /// column's name, an expression's leading function name, or `expr`
    /// for a non-function expression; INCLUDE columns contribute labels
    /// too. On a name clash within the relation an integer counter is
    /// appended (`_idx`, `_idx1`, `_idx2`, …).
    fn choose_auto_index_name(&self, stmt: &CreateIndexStatement) -> String {
        let mut labels: Vec<String> = Vec::new();
        match &stmt.expression {
            Some(Expr::FunctionCall { name, .. }) => labels.push(name.to_ascii_lowercase()),
            Some(_) => labels.push("expr".to_string()),
            None => labels.push(stmt.column.clone()),
        }
        labels.extend(stmt.extra_columns.iter().cloned());
        labels.extend(stmt.included_columns.iter().cloned());
        let mut base = alloc::format!("{}_{}_idx", stmt.table, labels.join("_"));
        // PG truncates the generated name to NAMEDATALEN-1 (63) bytes.
        truncate_ident(&mut base);
        // Collision counter — index names live in the relation's index
        // list (SPG keys index-name uniqueness per table), which is where
        // a same-column repeat collides, matching PG's observable output.
        let existing: Vec<String> = self
            .active_catalog()
            .get(&stmt.table)
            .map(|t| t.indices().iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default();
        if !existing.iter().any(|n| *n == base) {
            return base;
        }
        let mut counter = 1u32;
        loop {
            let mut cand = alloc::format!("{base}{counter}");
            truncate_ident(&mut cand);
            if !existing.iter().any(|n| *n == cand) {
                return cand;
            }
            counter += 1;
        }
    }

    pub(crate) fn exec_create_index(
        &mut self,
        mut stmt: CreateIndexStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (read01 round 93) — an omitted index name (`CREATE INDEX
        // ON t (a)`) is filled in with a PG-style generated name here, so
        // the name is chosen against the live catalog (for the collision
        // counter). Done before the partition-parent fan-out so children
        // inherit a fully-named template.
        if stmt.name.is_empty() {
            stmt.name = self.choose_auto_index_name(&stmt);
        }
        // v7.37.6-B(sentori Epic 2 P0)— `CREATE INDEX … ON parent`
        // when `parent` is a partition-parent fans out to every
        // existing child and records the Display-form source so
        // future children also build the same index at creation.
        // Parent itself holds no rows, so the build is skipped on
        // the parent table.
        if crate::partition::is_partition_parent(self.active_catalog(), &stmt.table) {
            return self.exec_create_index_on_partition_parent(stmt);
        }
        // v7.36 — collect cold-tier rows BEFORE taking the mutable
        // borrow on the table (the duplicate-scan post-CREATE UNIQUE
        // INDEX consumes them). `iter_cold_rows_of_parent` borrows
        // the catalog immutably so it would conflict with the
        // `active_catalog_mut` borrow below.
        let cold_rows_for_unique_scan: alloc::vec::Vec<spg_storage::Row> =
            if let Some(t) = self.active_catalog().get(&stmt.table) {
                crate::constraints::iter_cold_rows_of_parent(self.active_catalog(), t)
            } else {
                alloc::vec::Vec::new()
            };
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // `IF NOT EXISTS` reduces DuplicateIndex to a no-op CommandOk.
        if stmt.if_not_exists && table.indices().iter().any(|i| i.name == stmt.name) {
            // v7.39 (read01 round 46) — PG's IF NOT EXISTS skip NOTICE
            // (an index is a relation, so PG says "relation").
            self.notice(alloc::format!(
                "relation {:?} already exists, skipping",
                stmt.name
            ));
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        // v7.9.14 — multi-column index parses through; engine
        // builds a single-column BTree on the leading column only.
        // The trailing index columns are resolved + persisted below
        // (for every index, not just UNIQUE) so the catalog reports the
        // full column list; the BTree still keys on the leading column.
        let table_name = stmt.table.clone();
        // v6.8.0 — resolve INCLUDE column names to positions. Done
        // before `add_index` so a typo error surfaces before any
        // catalog mutation lands.
        let included_positions: Vec<usize> = if stmt.included_columns.is_empty() {
            Vec::new()
        } else {
            let schema = table.schema();
            stmt.included_columns
                .iter()
                .map(|c| {
                    schema.column_position(c).ok_or_else(|| {
                        EngineError::Storage(StorageError::ColumnNotFound { column: c.clone() })
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        // r1038 — an operator class that does not exist is refused here,
        // with PG's wording and its access method.
        //
        // The parser recognises an opclass by its position, so it no longer
        // rejects an unknown NAME as a syntax error the way its old
        // eighteen-name whitelist did as a side effect. That whitelist was
        // the sentori defect (`jsonb_path_ops` is ordinary PG and did not
        // parse); the refusal it was also doing belongs here, where the
        // access method is known and the error can carry it.
        if let Some(op) = &stmt.opclass
            && !crate::opclass::exists_for_access_method(op, stmt.method_name.as_deref())
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "operator class {op:?} does not exist for access method {:?}",
                stmt.method_name.as_deref().unwrap_or("btree")
            )));
        }
        // v7.39 (round 475) — an expression key a method cannot take is
        // refused BEFORE anything is built.
        //
        // The check used to run after the index was created, so
        // `CREATE INDEX gx ON g USING gin (to_tsvector('simple', doc))`
        // raised an error AND left a btree index named `gx` on `doc`
        // behind. The message said nothing had happened, the catalog said
        // otherwise, and a dump carried an index the user never wrote.
        let gin_fulltext_col = match (&stmt.expression, stmt.method) {
            (Some(e), IndexMethod::Gin) => tsvector_source_column(e),
            _ => None,
        };
        // v7.38.16 — a GIN index on an expression is PG's ordinary
        // spelling for full-text search, and SPG refused it outright:
        // `USING gin (to_tsvector('english', title || ' ' || body))` and
        // `USING gin (coalesce(title,''))` and `USING gin ((meta ->
        // 'tags'))` all failed the DDL, so a customer's schema did not
        // load at all. Only `to_tsvector(col)` worked, because
        // `tsvector_source_column` recognises a bare column as the last
        // argument and nothing else.
        //
        // The index kind follows the EXPRESSION's result type, since
        // there is no column whose type could decide it.
        let gin_expr_kind = match (&stmt.expression, stmt.method) {
            // Every GIN expression key, including `to_tsvector(col)`.
            // That one used to route to the MySQL FULLTEXT posting list,
            // which tokenises with the `simple` rule — so a query written
            // `to_tsvector('english', body) @@ to_tsquery('english','lazy')`
            // looked for the stem `lazi` in a list that held `lazy`, found
            // nothing, and returned NO ROWS where the same query without
            // the index returned one. Keying on the evaluated tsvector
            // puts the query's own configuration in the index.
            (Some(e), IndexMethod::Gin) => {
                crate::describe::describe_expr_type(e, &table.schema().columns)
            }
            _ => None,
        };
        if let Some(key_expr) = &stmt.expression
            && gin_fulltext_col.is_none()
            && gin_expr_kind.is_none()
            && matches!(
                stmt.method,
                IndexMethod::Hnsw | IndexMethod::Brin | IndexMethod::Gin
            )
        {
            // The old wording named HNSW and BRIN while also covering GIN,
            // so a refused GIN index reported two methods it was not.
            let method = match stmt.method {
                IndexMethod::Hnsw => "HNSW",
                IndexMethod::Brin => "BRIN",
                _ => "GIN",
            };
            return Err(EngineError::Unsupported(alloc::format!(
                "expression keys are not supported on {method} indexes: {key_expr}"
            )));
        }
        if let Some(ty) = gin_expr_kind {
            // The expression's own type picks the posting-list shape.
            // `column_position` still names the expression's leading
            // column so the catalog stays well-formed; the ENTRIES come
            // from `expr_index::refresh` below, never from that column.
            let anchor = stmt.column.clone();
            match ty {
                spg_storage::DataType::TsVector => table
                    .add_gin_index_on_expression(stmt.name.clone(), &anchor)
                    .map_err(EngineError::Storage)?,
                spg_storage::DataType::Json | spg_storage::DataType::Jsonb => table
                    .add_gin_jsonb_index(stmt.name.clone(), &anchor)
                    .map_err(EngineError::Storage)?,
                spg_storage::DataType::Text | spg_storage::DataType::Varchar(_) => table
                    .add_gin_trgm_index(stmt.name.clone(), &anchor)
                    .map_err(EngineError::Storage)?,
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "GIN cannot index an expression of type {ty:?}: {}",
                        stmt.expression.as_ref().map_or_else(
                            alloc::string::String::new,
                            alloc::string::ToString::to_string
                        )
                    )));
                }
            }
        } else if let Some(col) = gin_fulltext_col.clone() {
            table
                .add_gin_fulltext_index(stmt.name.clone(), &col)
                .map_err(EngineError::Storage)?;
        } else {
            match stmt.method {
                IndexMethod::BTree => {
                    table.add_index(stmt.name.clone(), &stmt.column)?;
                    // v7.38 P0 元机制 A — index has been pushed onto
                    // the table's index vector. Tests use this point
                    // to race a sealed index against a concurrent
                    // read.
                    crate::injection_point!("index_build_post_seal", &stmt.name);
                }
                IndexMethod::Hnsw => {
                    if !included_positions.is_empty() {
                        return Err(EngineError::Unsupported(
                            "INCLUDE columns are not supported on HNSW indexes".into(),
                        ));
                    }
                    table.add_nsw_index(
                        stmt.name.clone(),
                        &stmt.column,
                        spg_storage::NSW_DEFAULT_M,
                    )?;
                }
                // v6.7.1 — BRIN. Pure metadata; no in-memory data.
                IndexMethod::Brin => {
                    if !included_positions.is_empty() {
                        return Err(EngineError::Unsupported(
                            "INCLUDE columns are not supported on BRIN indexes".into(),
                        ));
                    }
                    table.add_brin_index(stmt.name.clone(), &stmt.column)?;
                }
                // v7.12.3 — GIN inverted index. Real posting-list-backed
                // GIN when the indexed column is `tsvector`; falls back
                // to a BTree on the leading column for any other column
                // type so v7.9.26b's `pg_dump` compatibility (GIN on
                // JSONB etc. silently loading as BTree) is preserved.
                // Operators see the real GIN only where it matters; old
                // schemas keep loading.
                IndexMethod::Gin => {
                    if !included_positions.is_empty() {
                        return Err(EngineError::Unsupported(
                            "INCLUDE columns are not supported on GIN indexes".into(),
                        ));
                    }
                    let col_pos =
                        table
                            .schema()
                            .column_position(&stmt.column)
                            .ok_or_else(|| {
                                EngineError::Storage(StorageError::ColumnNotFound {
                                    column: stmt.column.clone(),
                                })
                            })?;
                    let col_ty = table.schema().columns[col_pos].ty;
                    // v7.15.0 — `gin_trgm_ops` on a TEXT/VARCHAR
                    // column dispatches to the real trigram-shingle
                    // GIN build (LIKE / similarity acceleration).
                    // Other GIN opclasses fall through to the regular
                    // tsvector-vs-BTree split below.
                    let is_trgm = stmt
                        .opclass
                        .as_deref()
                        .is_some_and(|op| op.eq_ignore_ascii_case("gin_trgm_ops"));
                    if is_trgm
                        && matches!(
                            col_ty,
                            spg_storage::DataType::Text | spg_storage::DataType::Varchar(_)
                        )
                    {
                        table
                            .add_gin_trgm_index(stmt.name.clone(), &stmt.column)
                            .map_err(EngineError::Storage)?;
                    } else if col_ty == spg_storage::DataType::TsVector {
                        table
                            .add_gin_index(stmt.name.clone(), &stmt.column)
                            .map_err(EngineError::Storage)?;
                    } else if matches!(
                        col_ty,
                        spg_storage::DataType::Json | spg_storage::DataType::Jsonb
                    ) {
                        // v7.37.8(sentori Epic 5 P2)— real JSONB-GIN
                        // posting list. Pre-7.37.8 the same DDL loaded
                        // as a BTree fallback so `pg_dump` scripts that
                        // named GIN on JSONB stayed loadable but the
                        // posting-list acceleration was missing; the
                        // sentori dashboard's `labels @> '...'` queries
                        // fell back to full scan. The planner picks
                        // this index up via the `@>` seek in
                        // `index_access::try_gin_jsonb_seek`.
                        table
                            .add_gin_jsonb_index(stmt.name.clone(), &stmt.column)
                            .map_err(EngineError::Storage)?;
                    } else {
                        // v7.9.26b BTree fallback — the catalog still
                        // gets an index entry on the leading column so
                        // pg_dump scripts that name GIN on other column
                        // types load clean; query-time gain stays opt-in
                        // for tsvector / JSONB callers.
                        table.add_index(stmt.name.clone(), &stmt.column)?;
                    }
                }
            }
        }
        if !included_positions.is_empty()
            && let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name)
        {
            idx.included_columns = included_positions;
        }
        // v6.8.1 — persist partial-index predicate. Stored as the
        // expression's Display form so the catalog snapshot stays
        // pure (storage has no spg-sql dependency). The runtime
        // maintenance path treats partial indexes identically to
        // full indexes for v6.8.1 (over-maintenance is safe; the
        // planner-side "use partial when query WHERE implies the
        // predicate" pass is STABILITY carve-out).
        if let Some(pred_expr) = &stmt.partial_predicate {
            let canonical = pred_expr.to_string();
            // v7.13.2 — mailrs round-6 S2. PG's `pg_trgm` uses
            // `CREATE INDEX … USING gin(col gin_trgm_ops) WHERE …`
            // routinely to slim trigram indexes. SPG now persists
            // the predicate for GIN / BRIN / HNSW the same way it
            // already does for BTree — same v6.8.1 "over-maintain
            // is safe; planner-side partial routing is STABILITY
            // carve-out" semantics. HNSW carries an additional
            // caveat: the predicate isn't applied at index build
            // time (would require per-row eval inside the NSW
            // construction loop), so the index oversamples; query
            // time the WHERE clause still filters correctly.
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.partial_predicate = Some(canonical);
            }
        }
        // v6.8.2 — persist expression index key. Same Display-form
        // storage; the runtime maintenance pass evaluates each
        // row's expression to derive the index key, but for v6.8.2
        // the engine falls through to the bare-column-reference
        // path and the expression is preserved for format-layer
        // round-trip + future planner work. Carved-out in
        // STABILITY § "Out of v6.8".
        if let Some(key_expr) = &stmt.expression {
            // v7.39 (round 475) — the method check moved above, before
            // anything is built.
            let canonical = key_expr.to_string();
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.expression = Some(canonical);
            }
            // v7.38.16 — and now FILL it with the expression's values.
            // Until this call the B-tree holds the leading column's
            // values, which is what the index was built from and what no
            // lookup of `lower(s) = …` could ever match. `refresh` is a
            // no-op for a GIN full-text index, whose expression names a
            // source column that its own maintenance path already reads.
            crate::expr_index::refresh(table)?;
        }
        // v7.38.18 (S0) — and a locale-collated column index, for the
        // same reason: `Table::add_index` deliberately leaves its tree
        // EMPTY because only this crate can encode ICU sort keys, so
        // without this the index would exist, be skipped by every seek
        // (`Table::index_on` declines an incomplete one), and cost
        // maintenance for nothing.
        crate::expr_index::refresh(table)?;
        // v7.9.29 — persist `is_unique` flag on the storage Index.
        // Combined with `partial_predicate`, INSERT enforcement
        // checks that no other row whose predicate evaluates true
        // shares the same indexed key. Parser already rejected
        // `UNIQUE` on HNSW / BRIN, so plain BTree here.
        // Resolve the trailing index columns to positions and persist
        // them on EVERY index, unique or not — the BTree keys on the
        // leading column, but the extras drive uniqueness enforcement
        // (unique) and the catalog / pg_get_indexdef column list
        // (both), so a plain `CREATE INDEX t (a, b)` reports (a, b).
        {
            let mut extra_positions: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
            for col_name in &stmt.extra_columns {
                let pos = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "INDEX {:?}: extra column {col_name:?} not in table {:?}",
                            stmt.name,
                            stmt.table
                        ))
                    })?;
                extra_positions.push(pos);
            }
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.extra_column_positions = extra_positions;
            }
            // v7.38.1 (L12) — a multi-column CREATE INDEX becomes a REAL
            // composite B-tree: the key is the whole column tuple, so an
            // equality on any prefix seeks instead of filtering a
            // leading-column candidate flood. Expression / partial /
            // GIN-shaped indexes are declined inside and stay as built;
            // the indexdef already printed the full column list either
            // way, so nothing catalog-visible changes.
            table
                .convert_index_to_multi(&stmt.name)
                .map_err(EngineError::Storage)?;
        }
        // v7.39 (round 537) — the key column's ordering clause, as
        // written. It changes no lookup; `indexdef` reproduces the DDL,
        // and dropping it made `(a DESC NULLS LAST)` read back as `(a)`.
        if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
            idx.descending = stmt.key_order.descending;
            idx.nulls_first = stmt.key_order.nulls_first;
            idx.collation.clone_from(&stmt.key_collation);
        }
        if stmt.is_unique {
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.is_unique = true;
                // v7.39 (read01 round 52) — NULLS NOT DISTINCT (PG 15+).
                idx.nulls_not_distinct = stmt.nulls_not_distinct;
            }
            // At index-creation time, check the existing rows for
            // pre-existing duplicates that would have violated the
            // new constraint — otherwise CREATE UNIQUE INDEX would
            // silently leave duplicates in place.
            let snapshot_indices = table.indices().to_vec();
            let mut snapshot_rows: alloc::vec::Vec<spg_storage::Row> =
                table.rows().iter().cloned().collect();
            // v7.36 (cold-tier coverage) — CREATE UNIQUE INDEX must
            // detect a duplicate that would violate the new
            // uniqueness contract even when the duplicate is in the
            // cold tier; otherwise the constraint declaration
            // succeeds but the on-disk segments carry stale
            // duplicates and later INSERTs see phantom-conflict
            // behaviour. Use the catalog-borrowing variant from
            // `constraints` so we don't double-borrow `self` mut.
            snapshot_rows.extend(cold_rows_for_unique_scan);
            let snapshot_schema = table.schema().clone();
            let idx_ref = snapshot_indices
                .iter()
                .find(|i| i.name == stmt.name)
                .expect("just-added index");
            // v7.39 (read01 round 52) — the index was already installed above,
            // so a validation failure must ROLL IT BACK. PG's CREATE UNIQUE
            // INDEX is atomic; SPG used to leave the half-built index in the
            // catalog (pg_indexes listed an index that "failed" to create).
            if let Err(e) = check_existing_unique_violation(
                idx_ref,
                &snapshot_schema,
                &snapshot_rows,
                self.backslash_escapes,
            ) {
                let name = stmt.name.clone();
                self.active_catalog_mut().drop_named_index(&name);
                return Err(e);
            }
        }
        // v6.3.1 — adding an index can change the optimal plan for
        // any cached query that references this table.
        self.plan_cache.evict_referencing(&table_name);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.37.6-B(sentori Epic 2 P0)— `CREATE INDEX … ON parent`
    /// fans the index out to every existing child plus records
    /// the Display-form source so future children build it too.
    /// The parent itself stays index-less because it holds no rows.
    fn exec_create_index_on_partition_parent(
        &mut self,
        stmt: CreateIndexStatement,
    ) -> Result<QueryResult, EngineError> {
        let parent_name = stmt.table.clone();
        // Display-form source (round-trips through fmt::Display)
        // → store on parent's PartitionRole::Parent template list.
        let template_source = alloc::format!("{stmt}");
        let children = crate::partition::children_of_parent(self.active_catalog(), &parent_name);
        // Append the template to the parent schema before fanning
        // out, so a child whose CREATE FAILS halfway through still
        // records the template the user asked for. Idempotency is
        // handled at child-create time via `IF NOT EXISTS`.
        {
            let parent = self
                .active_catalog_mut()
                .get_mut(&parent_name)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: parent_name.clone(),
                    })
                })?;
            if let Some(PartitionRole::Parent {
                index_template_sources,
                ..
            }) = parent.schema_mut().partition_role.as_mut()
            {
                index_template_sources.push(template_source.clone());
            }
        }
        for child in children {
            self.execute_partition_index_template(&child, &template_source)?;
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.13.3 — mailrs round-7 S9. SPG-specific reconciliation
    /// for `CREATE TABLE IF NOT EXISTS` when the table already
    /// exists. Adds missing columns + inline FKs from the new
    /// definition; existing columns / constraints stay untouched.
    /// New columns with a `NOT NULL` declaration without a
    /// `DEFAULT` are reported as a clear error rather than
    /// silently dropped — this is the "fail loud on real
    /// incompatibility, fail silent on schema-superset" tradeoff.
    fn reconcile_table_if_not_exists(
        &mut self,
        stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        let table_name = stmt.name.clone();
        let clock = self.clock;
        let existing_col_names: alloc::collections::BTreeSet<String> = self
            .active_catalog()
            .get(&table_name)
            .expect("checked above")
            .schema()
            .columns
            .iter()
            .map(|c| c.name.to_ascii_lowercase())
            .collect();
        let row_count = self
            .active_catalog()
            .get(&table_name)
            .expect("checked above")
            .row_count();
        // Collect missing column defs in source order.
        let new_columns: alloc::vec::Vec<spg_sql::ast::ColumnDef> = stmt
            .columns
            .iter()
            .filter(|c| !existing_col_names.contains(&c.name.to_ascii_lowercase()))
            .cloned()
            .collect();
        for col_def in new_columns {
            let col_name = col_def.name.clone();
            let nullable = col_def.nullable;
            let has_default = col_def.default.is_some() || col_def.auto_increment;
            let col_schema = column_def_to_schema(col_def, self.backslash_escapes)?;
            let fill_value: Value<'static> = if has_default || col_schema.runtime_default.is_some()
            {
                resolve_column_default_free(&col_schema, clock, None)?
            } else if nullable || row_count == 0 {
                Value::Null
            } else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CREATE TABLE IF NOT EXISTS {table_name:?}: reconciling \
                     column {col_name:?} requires DEFAULT (existing rows would violate NOT NULL)"
                )));
            };
            let table = self
                .active_catalog_mut()
                .get_mut(&table_name)
                .expect("checked above");
            table.add_column(col_schema, fill_value);
        }
        // Resolve any newly-added inline FKs (column-level
        // REFERENCES forms) and install. Skip FKs whose local
        // columns we didn't have in the existing table.
        let table_cols_now = self
            .active_catalog()
            .get(&table_name)
            .expect("checked above")
            .schema()
            .columns
            .clone();
        for fk in stmt.foreign_keys {
            // Only install FKs whose every local column resolves
            // — older catalogs may have a column the new FK
            // references but not the column the new FK declares.
            let all_resolved = fk.columns.iter().all(|c| {
                table_cols_now
                    .iter()
                    .any(|sc| sc.name.eq_ignore_ascii_case(c))
            });
            if !all_resolved {
                continue;
            }
            let already_present = {
                let table = self
                    .active_catalog()
                    .get(&table_name)
                    .expect("checked above");
                table.schema().foreign_keys.iter().any(|f| {
                    f.parent_table.eq_ignore_ascii_case(&fk.parent_table)
                        && f.local_columns.len() == fk.columns.len()
                })
            };
            if already_present {
                continue;
            }
            let storage_fk =
                resolve_foreign_key(&table_name, &table_cols_now, fk, self.active_catalog())?;
            let table = self
                .active_catalog_mut()
                .get_mut(&table_name)
                .expect("checked above");
            table.schema_mut().foreign_keys.push(storage_fk);
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.14.0 — DROP TABLE handler (pg_dump / mysqldump preamble).
    pub(crate) fn exec_drop_table(
        &mut self,
        names: Vec<String>,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        for name in names {
            // v7.39 (round 642) — dropping a partition parent drops its
            // partitions with it.
            //
            // v7.37.6-B refused instead, on the premise that PG needs an
            // explicit CASCADE here. Measured on PG18, it does not: a
            // plain `DROP TABLE pp` takes pp and every partition, and so
            // does the CASCADE spelling. The refusal made the parent
            // undroppable by either spelling — `DROP TABLE IF EXISTS pp
            // CASCADE` at the head of a script failed, and every
            // statement after it failed on the leftovers.
            //
            // v7.39 (round 645) — inheritance is the other way round.
            // Measured on PG18: `DROP TABLE <inheritance parent>` with a
            // child is "cannot drop table par because other objects
            // depend on it / table ch depends on table par", and the
            // child survives. Only a PARTITION parent takes its children
            // with it.
            if crate::partition::has_inheritance_children(self.active_catalog(), &name) {
                let kids = crate::partition::children_of_parent(self.active_catalog(), &name);
                return Err(EngineError::Unsupported(alloc::format!(
                    "cannot drop table {name} because other objects depend on it\n\
                     DETAIL:  table {} depends on table {name}",
                    kids.first().map_or("?", |k| k.as_str())
                )));
            }
            // Depth-first: a partition may itself be partitioned, and
            // its children have to go before it does.
            let mut to_drop = alloc::vec::Vec::new();
            let mut frontier = alloc::vec![name.clone()];
            while let Some(cur) = frontier.pop() {
                for kid in crate::partition::children_of_parent(self.active_catalog(), &cur) {
                    frontier.push(kid.clone());
                    to_drop.push(kid);
                }
            }
            // Deepest first, so no parent is removed while a child of it
            // is still listed.
            for kid in to_drop.into_iter().rev() {
                let kid_was_temp = self.temp_tables.contains(&kid);
                if self.active_catalog_mut().drop_table(&kid) {
                    if kid_was_temp {
                        self.temp_tables.remove(&kid);
                        self.refresh_temp_prefix();
                    }
                    self.table_write_stats.remove(&kid);
                }
            }
            // v7.39 (round 436) — if this was one of the session's TEMPORARY
            // tables, forget it too, so a permanent namesake becomes visible
            // again and `end_session` does not chase a gone table.
            let was_temp = self.temp_tables.contains(&name);
            let dropped = self.active_catalog_mut().drop_table(&name);
            if dropped && was_temp {
                self.temp_tables.remove(&name);
                self.refresh_temp_prefix();
            }
            if dropped {
                // r192 — drop the non-transactional DML counters so a
                // later same-named table starts at zero (PG resets
                // stats on DROP).
                self.table_write_stats.remove(&name);
                // v7.39 (read01 round 50) — purge the table's comments (and its
                // columns') so a later table of the same name can't inherit them.
                self.active_catalog_mut().drop_comments_for("table", &name);
            }
            if !dropped {
                if !if_exists {
                    // v7.39 (read01 round 45) — PG wording (42P01 at the wire);
                    // PG says "table", not "relation", for DROP TABLE.
                    return Err(EngineError::Unsupported(alloc::format!(
                        "table {name:?} does not exist"
                    )));
                }
                // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                self.notice(alloc::format!("table {name:?} does not exist, skipping"));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.14.0 — DROP INDEX handler.
    pub(crate) fn exec_drop_index(
        &mut self,
        name: String,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let dropped = self.active_catalog_mut().drop_named_index(&name);
        if !dropped {
            if !if_exists {
                return Err(EngineError::Storage(StorageError::IndexNotFound { name }));
            }
            // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
            self.notice(alloc::format!("index {name:?} does not exist, skipping"));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    pub(crate) fn exec_create_table(
        &mut self,
        mut stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 436) — a TEMPORARY table is created under the calling
        // session's namespace prefix and remembered there, so it shadows a
        // permanent table of the same name, stays invisible to other
        // sessions, and goes away with the session. Everything downstream
        // (the whole DDL body, and every later statement) then works on an
        // ordinary table: name resolution happens at the ONE place a name
        // becomes an index, `Catalog::resolve_index`.
        if stmt.temporary {
            let logical = stmt.name.clone();
            let mangled = self.session_temp_name(&logical);
            let mut inner = stmt;
            inner.temporary = false;
            inner.name = mangled;
            let result = self.exec_create_table(inner)?;
            self.temp_tables.insert(logical);
            self.refresh_temp_prefix();
            return Ok(result);
        }
        if stmt.if_not_exists && self.active_catalog().get(&stmt.name).is_some() {
            // v7.39 (read01 round 46) — PG's IF NOT EXISTS skip NOTICE.
            self.notice(alloc::format!(
                "relation {:?} already exists, skipping",
                stmt.name
            ));
            // v7.16.2 — PG-strict silent no-op (mailrs round-10
            // surfaced this). v7.13.3's "reconcile by adding
            // missing columns" was friendly for mailrs round-7
            // where init-schema's `contacts` and migrate-023's
            // CardDAV `contacts` collided; but it ALSO silently
            // added columns to existing tables when later
            // migrations had a duplicate `CREATE TABLE IF NOT
            // EXISTS <t> (different-shape-cols)` shape. mailrs's
            // migrate-030 has exactly that — re-declares
            // system_config with `key` even though init-schema
            // already created it with `config_key`. PG's silent
            // no-op leaves system_config at `config_key`;
            // v7.13.3 added a phantom `key` column that then
            // tripped migrate-040's idempotent rename guard.
            // mailrs v1.7.106 ships the proper PG-style
            // contacts rename via DO + IF EXISTS, so SPG can
            // revert to PG-strict here without re-breaking the
            // round-7 case.
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        // v7.37.6-B(sentori Epic 2 P0)— `CREATE TABLE c PARTITION
        // OF parent <bounds>`: the child inherits its column list
        // from the parent and gets a `PartitionRole::Range` or
        // `Default` tag. Parent-table bookkeeping (index template
        // fan-out) runs in `register_partition_child`.
        if stmt.partition_of.is_some() {
            return self.exec_create_table_partition_of(stmt);
        }
        let table_name = stmt.name.clone();
        // v7.9.13 — pluck the names of any columns marked
        // `PRIMARY KEY` inline so the post-create-table pass can
        // build an implicit BTree index. mailrs F1.
        let inline_pk_columns: Vec<String> = stmt
            .columns
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| c.name.clone())
            .collect();
        let like_specs = core::mem::take(&mut stmt.like_specs);
        let mut schema = self.build_create_table_schema(
            &table_name,
            stmt.columns,
            &stmt.table_constraints,
            stmt.foreign_keys,
            &inline_pk_columns,
        )?;
        // v7.39 (round 531) — expand each `LIKE <table>` in the column
        // list. The source's shape lives in the catalog, so the parser
        // recorded the clause and it is copied here, at the position it
        // was written.
        let mut like_indexes: Vec<CreateIndexStatement> = Vec::new();
        self.apply_like_specs(&mut schema, &like_specs, &mut like_indexes)?;
        // v7.39 (round 645) — `INHERITS (p1, p2)`. Each parent's columns
        // land BEFORE the child's own, in the order the parents were
        // written, which is the order PG uses and the order
        // `pg_inherits.inhseqno` numbers them in.
        //
        // NOT NULL, DEFAULT and CHECK come with a column; PRIMARY KEY
        // and UNIQUE do not — measured on PG18, a child of a table with
        // a primary key has no `contype = 'p'` row of its own.
        //
        // A name the child also declares is not duplicated: PG merges
        // the two, keeping one column, and requires the types to agree.
        if !stmt.inherits.is_empty() {
            let mut merged: Vec<spg_storage::ColumnSchema> = Vec::new();
            for parent in &stmt.inherits {
                let Some(p) = self.active_catalog().get(parent) else {
                    return Err(EngineError::Storage(
                        spg_storage::StorageError::TableNotFound {
                            name: parent.clone(),
                        },
                    ));
                };
                for col in &p.schema().columns {
                    if merged
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&col.name))
                    {
                        continue;
                    }
                    if let Some(own) = schema
                        .columns
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(&col.name))
                        && own.ty != col.ty
                    {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{}\" inherited from \"{parent}\" has type {}                              but the child declares {}",
                            col.name,
                            crate::conversions::pg_type_name_for_error(col.ty),
                            crate::conversions::pg_type_name_for_error(own.ty)
                        )));
                    }
                    merged.push(col.clone());
                }
            }
            // The child's own columns follow, minus any the parents
            // already supplied.
            for col in &schema.columns {
                if !merged
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&col.name))
                {
                    merged.push(col.clone());
                }
            }
            schema.columns = merged;
            // v7.39 (round 646) — CHECK constraints inherit too. Measured
            // on PG18: a child of a table with `CHECK (a > 0)` gets its
            // own `contype = 'c'` row. PRIMARY KEY and UNIQUE do NOT —
            // the same probe reads 0 for `contype = 'p'` — so only the
            // checks are copied.
            //
            // A constraint the child already declares by the same name is
            // left alone; PG merges the two rather than carrying both.
            for parent in &stmt.inherits {
                let Some(p) = self.active_catalog().get(parent) else {
                    continue;
                };
                // The NAME travels with the constraint. An unnamed CHECK
                // is auto-named per table, so copying it as-is would give
                // the child `<child>_a_check` where PG reports the
                // parent's `<parent>_a_check` — measured in the violation
                // message, which is where a user meets the name. Resolve
                // the parent's name once and carry it explicitly.
                let names = crate::system_catalog::pg_check_connames(p, parent, &p.schema().checks);
                for (ci, (chk, name)) in p.schema().checks.iter().zip(names).enumerate() {
                    let dup = schema.checks.iter().any(|c| match (&c.name, &chk.name) {
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        _ => c.expr == chk.expr,
                    });
                    if !dup {
                        // A child copies the parent's constraint, validation
                        // state and all.
                        schema.checks.push(spg_storage::CheckConstraint {
                            name: Some(name),
                            expr: chk.expr.clone(),
                            validated: chk.validated,
                        });
                    }
                }
            }
            schema.partition_role = Some(spg_storage::PartitionRole::Inherits {
                parent_names: stmt.inherits.clone(),
            });
        }
        // v7.37.6-B — `CREATE TABLE p (...) PARTITION BY RANGE (key)`:
        // attach the parent role to the freshly-built schema before
        // it lands in the catalog. Key column must be TIMESTAMPTZ
        // at v7.37.6-B (the only sentori shape); other key types are
        // a phase-2 carve-out.
        if let Some(by) = stmt.partition_by {
            let kind = match by.kind {
                PartitionKindAst::Range => PartitionKind::Range,
                PartitionKindAst::List => PartitionKind::List,
                PartitionKindAst::Hash => PartitionKind::Hash,
            };
            let mut key_column_positions = Vec::with_capacity(by.key_columns.len());
            for col_name in &by.key_columns {
                let pos = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "PARTITION BY: key column {col_name:?} not in column list"
                        ))
                    })?;
                // v7.37.16 (16.1/16.2/16.6) — accept the typed PG
                // builtins per partition strategy:
                //   RANGE → TIMESTAMPTZ / TIMESTAMP / DATE / BIGINT
                //           / INTEGER / SMALLINT
                //   LIST  → BIGINT / INTEGER / SMALLINT / DATE / TEXT
                //   HASH  → BIGINT / INTEGER / SMALLINT / TEXT / DATE
                //           / TIMESTAMPTZ
                let key_ty = &schema.columns[pos].ty;
                let key_ok = matches!(
                    key_ty,
                    DataType::Timestamptz
                        | DataType::Timestamp
                        | DataType::Date
                        | DataType::BigInt
                        | DataType::Int
                        | DataType::SmallInt
                        | DataType::Text
                        | DataType::Varchar(_)
                );
                if !key_ok {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION BY {:?}: key column {col_name:?} type {key_ty:?} \
                         is not yet supported (16.1/16.2/16.6 accept TIMESTAMPTZ, \
                         TIMESTAMP, DATE, BIGINT, INTEGER, SMALLINT, TEXT/VARCHAR)",
                        kind,
                    )));
                }
                key_column_positions.push(pos);
            }
            schema.partition_role = Some(PartitionRole::Parent {
                kind,
                key_column_positions,
                index_template_sources: Vec::new(),
            });
        }
        self.active_catalog_mut().create_table(schema)?;
        // v7.39 (round 621) — the indexes an `INCLUDING INDEXES` asked for,
        // created once the table they sit on exists.
        for mut ci in like_indexes {
            ci.table = table_name.clone();
            self.exec_create_index(ci)?;
        }
        self.install_implicit_indexes(&table_name, &inline_pk_columns, &stmt.table_constraints)?;
        self.install_excl_range_indexes(&table_name);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.37.6-B — child-table branch of `CREATE TABLE`. The parser
    /// guarantees `stmt.partition_of.is_some()` + `stmt.columns`
    /// is empty before we land here.
    fn exec_create_table_partition_of(
        &mut self,
        stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        let spec = stmt
            .partition_of
            .expect("caller checked partition_of.is_some()");
        // Lift parent schema bits (columns + partition_role + index
        // template list) so we don't trip the active_catalog_mut()
        // borrow when we splice the child in.
        let (parent_columns, parent_kind, index_template_sources) = {
            let parent = self
                .active_catalog()
                .get(&spec.parent_name)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: spec.parent_name.clone(),
                    })
                })?;
            match &parent.schema().partition_role {
                Some(PartitionRole::Parent {
                    kind,
                    index_template_sources,
                    ..
                }) => (
                    parent.schema().columns.clone(),
                    *kind,
                    index_template_sources.clone(),
                ),
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CREATE TABLE … PARTITION OF: table {:?} is not a \
                         partitioned parent",
                        spec.parent_name
                    )));
                }
            }
        };
        // Resolve bounds before we mutate the catalog so a bad
        // literal surfaces before any visible state changes.
        let role = match spec.bounds {
            PartitionOfBoundsAst::Default => PartitionRole::Default {
                parent_name: spec.parent_name.clone(),
            },
            PartitionOfBoundsAst::Range { lower, upper } => {
                let lower_b = crate::partition::evaluate_partition_bound(*lower)?;
                let upper_b = crate::partition::evaluate_partition_bound(*upper)?;
                // Half-open: lower must be < upper. Same-bound or
                // inverted ranges accept no rows in PG; SPG raises
                // because every sentori migration shapes intentional
                // calendar windows.
                if !crate::partition::ranges_overlap(&lower_b, &upper_b, &lower_b, &upper_b) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION OF: FROM ({}) TO ({}) is empty (lower must be < upper)",
                        crate::partition::bound_to_diag(&lower_b),
                        crate::partition::bound_to_diag(&upper_b),
                    )));
                }
                // Overlap check against every existing sibling Range
                // child of the same parent. DEFAULT siblings don't
                // participate(they're a catch-all, not a range).
                let siblings =
                    crate::partition::children_of_parent(self.active_catalog(), &spec.parent_name);
                // Partition-key column of the parent (RANGE uses one key).
                let key_pos = match &self
                    .active_catalog()
                    .get(&spec.parent_name)
                    .and_then(|p| p.schema().partition_role.clone())
                {
                    Some(PartitionRole::Parent {
                        key_column_positions,
                        ..
                    }) => key_column_positions.first().copied().unwrap_or(0),
                    _ => 0,
                };
                for sib in &siblings {
                    let Some(t) = self.active_catalog().get(sib) else {
                        continue;
                    };
                    match &t.schema().partition_role {
                        Some(PartitionRole::Range {
                            lower: sl,
                            upper: su,
                            ..
                        }) => {
                            if crate::partition::ranges_overlap(&lower_b, &upper_b, sl, su) {
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "PARTITION OF: range FROM ({}) TO ({}) overlaps existing \
                                     child {sib:?} (FROM ({}) TO ({}))",
                                    crate::partition::bound_to_diag(&lower_b),
                                    crate::partition::bound_to_diag(&upper_b),
                                    crate::partition::bound_to_diag(sl),
                                    crate::partition::bound_to_diag(su),
                                )));
                            }
                        }
                        // v7.38 (read01) — DEFAULT-partition cross-check:
                        // any row already parked in the default partition
                        // that falls in the new range means adding it would
                        // strand that row in the wrong partition. PG rejects
                        // rather than allow the inconsistency.
                        Some(PartitionRole::Default { .. }) => {
                            for row in t.rows().iter() {
                                let Some(v) = row.values.get(key_pos) else {
                                    continue;
                                };
                                if v.is_null() {
                                    continue;
                                }
                                let Some(kb) = crate::partition::value_to_bound(v) else {
                                    continue;
                                };
                                if crate::partition::value_in_range(&kb, &lower_b, &upper_b) {
                                    return Err(EngineError::Unsupported(alloc::format!(
                                        "updated partition constraint for default partition \
                                         {sib:?} would be violated by some row"
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                PartitionRole::Range {
                    parent_name: spec.parent_name.clone(),
                    lower: lower_b,
                    upper: upper_b,
                }
            }
            // v7.37.16 (16.1) — LIST child create.
            PartitionOfBoundsAst::List { values } => {
                if !matches!(parent_kind, PartitionKind::List) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION OF: FOR VALUES IN (...) only valid for \
                         a LIST-partitioned parent (parent {:?} is {:?})",
                        spec.parent_name,
                        parent_kind,
                    )));
                }
                let mut bounds = Vec::with_capacity(values.len());
                for v in values {
                    bounds.push(crate::partition::evaluate_partition_bound(v)?);
                }
                // Reject duplicate values across siblings (PG raises
                // "is already specified in partition X" at create
                // time so the dispatch never sees ambiguity).
                let siblings =
                    crate::partition::children_of_parent(self.active_catalog(), &spec.parent_name);
                for sib in &siblings {
                    let Some(t) = self.active_catalog().get(sib) else {
                        continue;
                    };
                    if let Some(PartitionRole::List {
                        values: existing, ..
                    }) = &t.schema().partition_role
                    {
                        for new_b in &bounds {
                            if existing.iter().any(|e| e == new_b) {
                                // v7.39 (round 770, F31 tranche 6 #170) —
                                // PG's sentence, measured: `partition "b"
                                // would overlap partition "a"`.
                                let _ = crate::partition::bound_to_diag(new_b);
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "partition \"{}\" would overlap partition \"{sib}\"",
                                    stmt.name,
                                )));
                            }
                        }
                    }
                }
                PartitionRole::List {
                    parent_name: spec.parent_name.clone(),
                    values: bounds,
                }
            }
            // v7.37.16 (16.2) — HASH child create.
            PartitionOfBoundsAst::Hash { modulus, remainder } => {
                if !matches!(parent_kind, PartitionKind::Hash) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION OF: FOR VALUES WITH (MODULUS, REMAINDER) only \
                         valid for a HASH-partitioned parent (parent {:?} is {:?})",
                        spec.parent_name,
                        parent_kind,
                    )));
                }
                if modulus == 0 || remainder >= modulus {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION OF HASH: invalid (MODULUS={modulus}, REMAINDER={remainder}); \
                         require modulus > 0 and remainder < modulus",
                    )));
                }
                // Reject duplicate (modulus, remainder) and partial overlap
                // (different modulus / same residue class) — PG handles
                // multi-modulus by requiring divisibility; we keep it
                // simple and demand modulus equality across HASH siblings.
                let siblings =
                    crate::partition::children_of_parent(self.active_catalog(), &spec.parent_name);
                for sib in &siblings {
                    let Some(t) = self.active_catalog().get(sib) else {
                        continue;
                    };
                    if let Some(PartitionRole::Hash {
                        modulus: m,
                        remainder: r,
                        ..
                    }) = &t.schema().partition_role
                    {
                        if *m != modulus {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "PARTITION OF HASH: MODULUS {modulus} differs from \
                                 sibling {sib:?} MODULUS {m} (mixed moduli not yet \
                                 supported in v7.37.16.2)",
                            )));
                        }
                        if *r == remainder {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "PARTITION OF HASH: REMAINDER {remainder} already \
                                 used by sibling {sib:?}",
                            )));
                        }
                    }
                }
                PartitionRole::Hash {
                    parent_name: spec.parent_name.clone(),
                    modulus,
                    remainder,
                }
            }
        };
        // For DEFAULT children, reject when the parent already has
        // one(PG semantics — exactly 0 or 1 DEFAULT per parent).
        if matches!(role, PartitionRole::Default { .. }) {
            for sib in
                crate::partition::children_of_parent(self.active_catalog(), &spec.parent_name)
            {
                if let Some(t) = self.active_catalog().get(&sib)
                    && matches!(
                        t.schema().partition_role,
                        Some(PartitionRole::Default { .. })
                    )
                {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "PARTITION OF DEFAULT: parent {:?} already has a DEFAULT \
                         partition ({sib:?})",
                        spec.parent_name
                    )));
                }
            }
        }
        let _ = parent_kind; // v7.37.6-B locks RANGE; future kinds key off this.
        let mut schema = TableSchema::new(stmt.name.clone(), parent_columns);
        // v7.39 (read01 round 57) — whoever runs CREATE TABLE owns it.
        schema.owner = Some(alloc::string::String::from(self.current_role()));
        schema.partition_role = Some(role);
        self.active_catalog_mut().create_table(schema)?;
        // Replay parent's CREATE INDEX templates against the new
        // child so every parent-declared index materialises now.
        for tmpl in &index_template_sources {
            self.execute_partition_index_template(&stmt.name, tmpl)?;
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.37.6-B — parse a stored `CREATE INDEX ON parent (…)`
    /// template and re-execute it against `child_name`(by rewriting
    /// the table reference on the AST before dispatch). Used both
    /// at child-create time and after `CREATE INDEX ON parent` for
    /// existing children.
    fn execute_partition_index_template(
        &mut self,
        child_name: &str,
        template_source: &str,
    ) -> Result<(), EngineError> {
        let stmt = spg_sql::parser::parse_statement(template_source).map_err(EngineError::Parse)?;
        let Statement::CreateIndex(mut ci) = stmt else {
            return Err(EngineError::Unsupported(alloc::format!(
                "PARTITION index template is not CREATE INDEX: {template_source:?}"
            )));
        };
        ci.table = child_name.to_string();
        // Name suffix per child so different children don't collide
        // on the same `<idx_name>`. Skip when the original index has
        // no explicit name(SPG auto-generates).
        if !ci.name.is_empty() {
            ci.name = alloc::format!("{}__{}", ci.name, child_name);
        }
        // IF NOT EXISTS to make replay idempotent — when this is
        // called from the CREATE INDEX ON parent fan-out we want to
        // tolerate the case where a child already has the index
        // from an earlier CREATE INDEX run.
        ci.if_not_exists = true;
        self.exec_create_index(ci)?;
        Ok(())
    }

    /// Build the `TableSchema` for a CREATE TABLE: column schemas with
    /// ENUM / DOMAIN bindings resolved, table-level + inline PRIMARY KEY
    /// NOT NULL marking, FK resolution (deferring to `pending_foreign_keys`
    /// when checks are off and the parent is absent), and uniqueness /
    /// CHECK constraint translation.
    #[allow(clippy::too_many_lines)]
    /// v7.39 (round 531) — copy a source table's shape into the new one.
    ///
    /// Measured on PG18: a bare `LIKE` copies names, types and NOT NULL
    /// and nothing else — a copied generated column becomes a plain one
    /// and a copied identity column loses its identity. Each INCLUDING
    /// adds one property back, and `INCLUDING ALL` adds them all.
    #[allow(clippy::too_many_lines)]
    fn apply_like_specs(
        &mut self,
        schema: &mut spg_storage::TableSchema,
        specs: &[spg_sql::ast::LikeSpec],
        out_indexes: &mut Vec<CreateIndexStatement>,
    ) -> Result<(), EngineError> {
        // Applied back to front so an earlier spec's insert position is
        // still the one it was written at.
        for spec in specs.iter().rev() {
            let src = self.active_catalog().get(&spec.source).ok_or_else(|| {
                EngineError::Storage(spg_storage::StorageError::TableNotFound {
                    name: spec.source.clone(),
                })
            })?;
            let src_schema = src.schema();
            let o = spec.options;
            let mut copied: Vec<spg_storage::ColumnSchema> = Vec::new();
            for c in &src_schema.columns {
                let mut col = c.clone();
                if !o.defaults {
                    col.default = None;
                    col.default_text = None;
                    col.runtime_default = None;
                }
                if !o.identity {
                    col.auto_increment = false;
                    col.identity_always = false;
                    col.auto_restart = None;
                }
                if !o.generated {
                    col.generated_stored_expr = None;
                }
                if !o.comments {
                    // Comments live in the catalog's comment map, not on
                    // the column, so there is nothing to clear here; the
                    // copy below simply does not carry them.
                }
                copied.push(col);
            }
            let at = spec.at.min(schema.columns.len());
            for (i, col) in copied.into_iter().enumerate() {
                schema.columns.insert(at + i, col);
            }
            if o.constraints {
                for chk in &src_schema.checks {
                    schema.checks.push(chk.clone());
                }
            }
            // v7.39 (round 621) — INCLUDING INDEXES copies them.
            //
            // Round 531 refused it rather than dropping them silently, and the
            // reason it gave was right: "a table that reports the right columns
            // and none of the indexes is the shape that looks fine until it is
            // slow". But refusing takes `INCLUDING ALL` down with it, which is
            // what schema tools write, so the restore stopped instead.
            //
            // The index is rebuilt from its own definition rather than copied
            // as a structure, so it goes through the same path a written-out
            // CREATE INDEX takes. PG names the copies after the new table and
            // lets the auto-namer resolve collisions, which is what an empty
            // name asks for here.
            if o.indexes {
                for idx in src.indices() {
                    let Some(col) = src_schema.columns.get(idx.column_position) else {
                        continue;
                    };
                    out_indexes.push(CreateIndexStatement {
                        concurrently: false,
                        name: String::new(),
                        key_order: spg_sql::ast::IndexColumnOrder::default(),
                        key_collation: None,
                        table: String::new(),
                        column: col.name.clone(),
                        nulls_not_distinct: idx.nulls_not_distinct,
                        method: spg_sql::ast::IndexMethod::BTree,
                        if_not_exists: false,
                        included_columns: Vec::new(),
                        partial_predicate: None,
                        expression: None,
                        extra_columns: Vec::new(),
                        is_unique: idx.is_unique,
                        opclass: None,
                        method_name: None,
                    });
                }
            }
        }
        Ok(())
    }

    fn build_create_table_schema(
        &mut self,
        table_name: &str,
        columns: Vec<ColumnDef>,
        table_constraints: &[spg_sql::ast::TableConstraint],
        foreign_keys: Vec<spg_sql::ast::ForeignKeyConstraint>,
        inline_pk_columns: &[String],
    ) -> Result<TableSchema, EngineError> {
        // v7.39 (round 711) — the inline PK's timing clause, captured
        // before `columns` is consumed into the schema below.
        let inline_pk_timing: (bool, bool) =
            columns
                .iter()
                .filter(|c| c.is_primary_key)
                .fold((false, false), |acc, c| {
                    (
                        acc.0 | c.constraint_deferrable,
                        acc.1 | c.constraint_initially_deferred,
                    )
                });
        // v7.9.19 — table-level constraints: PRIMARY KEY (a, b, ...)
        // and UNIQUE (a, b, ...). Each builds a BTree index on the
        // leading column (the existing single-column storage tier)
        // and registers a UniquenessConstraint on the schema for
        // INSERT-time enforcement of the full tuple. mailrs G1/G6.
        let mysql = self.backslash_escapes;
        let cols = columns
            .into_iter()
            .map(|c| column_def_to_schema(c, mysql))
            .collect::<Result<Vec<_>, _>>()?;
        // v7.39 (round 679) — say so when a declared collation is stored but
        // not applied.
        //
        // Round 670 measured three rules colliding here: refusing the DDL
        // breaks a customer's pg_dump restore (zero-customer-change), while
        // accepting it silently is what F36 records as the defect — the
        // declaration taken and ignored. A WARNING is the option that was
        // not available then: rounds 676-677 gave the name somewhere to
        // live, and round 678 gave `collate::is_supported` a way to say
        // whether this build can perform it. The restore still succeeds;
        // the gap stops being silent.
        //
        // SPG performs C and POSIX, so those warn about nothing.
        for c in &cols {
            let Some(name) = c.collation_name.as_deref() else {
                continue;
            };
            // v7.38.22 — the type has to be able to carry one.
            //
            // PostgreSQL 18.4 refuses `CREATE TABLE t (c INT COLLATE
            // "en_US.utf8")` with 42804; SPG took the declaration and
            // stored it, which is the same "taken and ignored" shape F36
            // was opened for, one level up — and it then travels into
            // every comparison the column takes part in.
            if !crate::collate::is_collatable(&c.ty) {
                return Err(crate::collate::not_collatable_error(
                    crate::eval::pg_typeof_name_for_datatype(c.ty).unwrap_or("unknown"),
                ));
            }
            if crate::collate::is_supported(name)
                && (name.eq_ignore_ascii_case("C")
                    || name.eq_ignore_ascii_case("POSIX")
                    || name.eq_ignore_ascii_case("default"))
            {
                continue;
            }
            // v7.39 (round 692) — the message says what is true TODAY.
            // Rounds 683–692 made ORDER BY, DISTINCT, GROUP BY, joins,
            // min/max and window ordering follow a declared collation, so
            // the old wording ("orders this column by bytes") had become
            // the wrong warning — and a wrong warning is worse than none,
            // because a customer reads it and plans around it.
            //
            // What is still true is the range comparison: `BETWEEN`, `<`,
            // `>` go through `binop::compare`, which takes two values and
            // no column. That one is not wiring; it needs collation
            // derivation at a comparison, and `compare` is the dominant
            // cost of a scan, so it needs a bench with it.
            if !crate::collate::is_known(name) {
                // v7.38.18 (G2) — see the ALTER site: PG 18.4 refuses a
                // name that is not in its catalogue, and so does this.
                return Err(crate::collate::unknown_collation_error(name));
            }
            if !crate::collate::is_supported(name) {
                self.warning(alloc::format!(
                    "column \"{}\" declares COLLATE \"{name}\", which this build cannot \
                     perform; SPG records the declaration and orders this column by bytes \
                     (the C collation)",
                    c.name
                ));
            }
        }
        // v7.17.0 Phase 1.4 + 1.5 — classify every raw
        // user_type_ref (parked as user_enum_type by
        // column_def_to_schema) into either an enum binding or a
        // domain binding. For domains, also rewrite the column's
        // base DataType from the placeholder Text to the domain's
        // declared base. Unknown idents are still a hard error
        // here (same as Phase 1.4) so silent acceptance never
        // happens.
        let mut cols = cols;
        for col in cols.iter_mut() {
            let Some(name) = col.user_enum_type.take() else {
                continue;
            };
            let cat = self.active_catalog();
            if cat.enum_types().contains_key(&name) {
                col.user_enum_type = Some(name);
                continue;
            }
            if let Some(dom) = cat.domain_types().get(&name) {
                let base_type = dom.base_type;
                let dom_default = dom.default.clone();
                col.ty = base_type;
                col.user_domain_type = Some(name);
                if !dom.nullable {
                    col.nullable = false;
                }
                // v7.39 (round 259) — two DEFAULT problems on a domain
                // column, both because the column was typed Text (the
                // parser's placeholder for an unknown type name) while its
                // DEFAULT was being resolved, and only re-typed here:
                //   * a COLUMN-level default failed to coerce and the
                //     whole CREATE TABLE errored ("type mismatch") — a
                //     hard failure on valid SQL;
                //   * the DOMAIN's own default was never adopted, so an
                //     omitted column landed NULL where PG gives the
                //     domain default (probed: 42, and a column default
                //     of 7 overrides it).
                if let Some(d) = col.default.take() {
                    col.default = Some(crate::conversions::coerce_value(
                        d, base_type, &col.name, 0,
                    )?);
                } else if let Some(src) = dom_default {
                    let expr = spg_sql::parser::parse_expression(&src).map_err(|e| {
                        EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                            "domain default {src:?} failed to re-parse: {e:?}"
                        )))
                    })?;
                    let empty: alloc::vec::Vec<spg_storage::ColumnSchema> = alloc::vec::Vec::new();
                    let ctx = crate::eval::EvalContext::new(&empty, None);
                    let row = spg_storage::Row {
                        values: alloc::vec::Vec::new(),
                    };
                    let v = crate::eval::eval_expr(&expr, &row, &ctx).map_err(EngineError::Eval)?;
                    col.default = Some(crate::conversions::coerce_value(
                        v, base_type, &col.name, 0,
                    )?);
                }
                continue;
            }
            // v7.37.42-T2 ζ-B — composite type bound to a column.
            // Stored as JSONB at the storage tier (positional + named
            // field access via JSONB path operators is the canonical
            // PG-compatible surface until Value::Composite lands).
            // The composite identity stays in `catalog.composite_types`
            // for introspection / DROP TYPE / column-type-DDL
            // round-trip.
            if cat.composite_types().contains_key(&name) {
                // v7.39 (read01 round 56) — the on-disk form stays JSONB, but
                // the column now RECORDS which composite type it holds. The
                // engine rehydrates the stored JSON into a Value::Composite on
                // read, so field access / ROW comparison / ordering / the
                // canonical `(2,b)` text form all work — every one of those was
                // already implemented on Value::Composite; the column simply
                // never remembered its type.
                col.ty = spg_storage::DataType::Jsonb;
                col.user_composite_type = Some(name.clone());
                continue;
            }
            // v7.38.19 — a PSEUDO-type is a different refusal. The name
            // exists; it just cannot hold a value, which PG reports as an
            // INVALID TABLE DEFINITION (42P16) naming the column rather
            // than an undefined type (42704) naming the type.
            if let Some(pseudo) = crate::conversions::pseudo_type(&name) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "column \"{}\" has pseudo-type {pseudo}",
                    col.name
                )));
            }
            // v7.39 (read01 round 89) — PG's 42704 wording. The old
            // "column X: unknown column type Y (...)" carried SPG's own
            // vocabulary and fell to the generic error class; PG says
            // simply `type "Y" does not exist`.
            return Err(EngineError::Unsupported(alloc::format!(
                "type \"{name}\" does not exist"
            )));
        }
        for tc in table_constraints {
            if let spg_sql::ast::TableConstraint::PrimaryKey { columns, .. } = tc {
                for col_name in columns {
                    if let Some(col) = cols.iter_mut().find(|c| c.name == *col_name) {
                        col.nullable = false;
                    }
                }
            }
        }
        // v7.6.1 — resolve every FK in the statement against the
        // already-known catalog. Validates: parent table exists,
        // parent column names exist, arity matches, parent columns
        // have a PK / UNIQUE index. Self-referencing FKs (parent
        // table == this table) resolve against the column list we
        // just built — they don't need the catalog yet.
        let mut fks: Vec<spg_storage::ForeignKeyConstraint> =
            Vec::with_capacity(foreign_keys.len());
        for fk in foreign_keys {
            // v7.14.0 — when SET FOREIGN_KEY_CHECKS=0 is in effect
            // (mysqldump preamble + bulk imports), defer FK
            // resolution if the parent table isn't in the catalog
            // yet. The FK is queued and resolved when checks flip
            // back on. Self-references stay in-band (the parent is
            // the same as the child we're building).
            let needs_parent = !fk.parent_table.eq_ignore_ascii_case(table_name);
            if !self.foreign_key_checks
                && needs_parent
                && self.active_catalog().get(&fk.parent_table).is_none()
            {
                self.pending_foreign_keys.push((table_name.to_string(), fk));
                continue;
            }
            fks.push(resolve_foreign_key(
                table_name,
                &cols,
                fk,
                self.active_catalog(),
            )?);
        }
        let mut schema = TableSchema::new(table_name.to_string(), cols);
        // v7.39 (read01 round 57) — whoever runs CREATE TABLE owns it (PG
        // `pg_class.relowner`); the owner holds every privilege implicitly.
        schema.owner = Some(alloc::string::String::from(self.current_role()));
        schema.foreign_keys = fks;
        // v7.9.19 — translate AST table_constraints to storage
        // UniquenessConstraints (column name → position) so the
        // INSERT enforcement helper sees positions directly.
        let mut uc_storage: Vec<spg_storage::UniquenessConstraint> = Vec::new();
        // v7.39 (read01 round 48) — the AST has carried `name` all along;
        // the schema now keeps it instead of dropping it on the floor.
        let mut check_exprs: Vec<spg_storage::CheckConstraint> = Vec::new();
        // v7.39 (round 210) — EXCLUDE constraints translate column names to
        // positions and synthesise PG's `<table>_<leading-col>_excl` name
        // when the user left it unnamed.
        let mut excl_storage: Vec<spg_storage::ExclusionConstraint> = Vec::new();
        for tc in table_constraints {
            let (is_pk, names, nnd, con_name, timing) = match tc {
                spg_sql::ast::TableConstraint::PrimaryKey {
                    name,
                    columns,
                    deferrable,
                    initially_deferred,
                } => (
                    true,
                    columns.clone(),
                    false,
                    name.clone(),
                    (*deferrable, *initially_deferred),
                ),
                spg_sql::ast::TableConstraint::Unique {
                    name,
                    columns,
                    nulls_not_distinct,
                    deferrable,
                    initially_deferred,
                } => (
                    false,
                    columns.clone(),
                    *nulls_not_distinct,
                    name.clone(),
                    (*deferrable, *initially_deferred),
                ),
                spg_sql::ast::TableConstraint::Check { name, expr, .. } => {
                    // v7.13.0 — collect CHECK predicate sources;
                    // they get attached to the schema below.
                    // A CREATE TABLE CHECK has no rows to grandfather; the
                    // parser refuses NOT VALID there, as PG does, so every
                    // one of these is validated and none needs a mark.
                    check_exprs.push(spg_storage::CheckConstraint {
                        name: name.clone(),
                        expr: alloc::format!("{expr}"),
                        validated: true,
                    });
                    continue;
                }
                spg_sql::ast::TableConstraint::Exclude {
                    name,
                    method,
                    elements,
                } => {
                    let mut els = Vec::with_capacity(elements.len());
                    for (col, op) in elements {
                        let pos = schema
                            .columns
                            .iter()
                            .position(|c| c.name == *col)
                            .ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "EXCLUDE constraint references unknown column {col:?}"
                                ))
                            })?;
                        els.push((pos, op.clone()));
                    }
                    // v7.39 (round 211) — PG auto-names an unnamed EXCLUDE
                    // `<table>_<col…>_excl`, joining ALL element columns
                    // (e.g. `book_room_during_excl`), not just the leading one.
                    let cols_joined = elements
                        .iter()
                        .map(|(c, _)| c.clone())
                        .collect::<Vec<_>>()
                        .join("_");
                    let con_name = name
                        .clone()
                        .unwrap_or_else(|| alloc::format!("{table_name}_{cols_joined}_excl"));
                    excl_storage.push(spg_storage::ExclusionConstraint {
                        name: con_name,
                        method: method.clone(),
                        elements: els,
                    });
                    continue;
                }
                // v7.15.0 — plain `KEY (cols)` from MySQL inline
                // is NOT a uniqueness constraint; skip the UC
                // build path entirely. The BTree index lands in
                // the post-create loop below alongside the PK/UQ
                // implicit indexes.
                spg_sql::ast::TableConstraint::Index { .. } => continue,
                // v7.17.0 Phase 2.2 — MySQL FULLTEXT KEY is not
                // a uniqueness constraint either; its GIN gets
                // built in the post-create loop below.
                spg_sql::ast::TableConstraint::FulltextIndex { .. } => continue,
            };
            let mut positions = Vec::with_capacity(names.len());
            for n in &names {
                let pos = schema
                    .columns
                    .iter()
                    .position(|c| c.name == *n)
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "table constraint references unknown column {n:?}"
                        ))
                    })?;
                positions.push(pos);
            }
            uc_storage.push(spg_storage::UniquenessConstraint {
                is_primary_key: is_pk,
                columns: positions,
                nulls_not_distinct: nnd,
                name: con_name,
                deferrable: timing.0,
                initially_deferred: timing.1,
            });
        }
        // v7.24 (round-16 collateral) — inline `PRIMARY KEY` column
        // constraints used to build only the implicit BTree index;
        // uniqueness was NEVER registered, so duplicate keys were
        // silently accepted (table-level PRIMARY KEY did enforce).
        // Register the same UniquenessConstraint the table-level
        // form gets, unless one already covers the column set.
        if !inline_pk_columns.is_empty() {
            let mut positions = Vec::with_capacity(inline_pk_columns.len());
            for n in inline_pk_columns {
                if let Some(pos) = schema.columns.iter().position(|c| c.name == *n) {
                    positions.push(pos);
                }
            }
            if !uc_storage
                .iter()
                .any(|uc| uc.is_primary_key || uc.columns == positions)
            {
                uc_storage.push(spg_storage::UniquenessConstraint {
                    is_primary_key: true,
                    columns: positions,
                    nulls_not_distinct: false,
                    deferrable: inline_pk_timing.0,
                    initially_deferred: inline_pk_timing.1,
                    // Inline `col INT PRIMARY KEY` carries no name.
                    name: None,
                });
            }
        }
        schema.uniqueness_constraints = uc_storage.clone();
        schema.checks = check_exprs;
        schema.exclusion_constraints = excl_storage;
        Ok(schema)
    }

    /// Install the implicit BTree / fulltext-GIN indexes a freshly-created
    /// table needs: one per inline PRIMARY KEY column, plus one per
    /// v7.39 (round 215) — build a range-overlap index for every EXCLUDE
    /// constraint whose `&&` element sits on an integer-keyable range column
    /// (int4/int8/date/ts/tstz range). Turns the O(n) enforcement scan into an
    /// O(log n) predecessor+successor probe. Idempotent — safe to call again
    /// after ALTER or on catalog load. Constraints the index can't cover
    /// (numrange, `@>`/`<@`/geometry operators) simply get no index and keep
    /// the correct O(n) scan.
    pub(crate) fn install_excl_range_indexes(&mut self, table_name: &str) {
        let Some(table) = self.active_catalog_mut().get_mut(table_name) else {
            return;
        };
        let cols: Vec<usize> = table
            .schema()
            .exclusion_constraints
            .iter()
            .filter_map(|ex| excl_index_column(table.schema(), ex))
            .collect();
        for c in cols {
            table.ensure_excl_range_index(c);
        }
    }

    /// table-level PRIMARY KEY / UNIQUE / KEY / FULLTEXT constraint.
    fn install_implicit_indexes(
        &mut self,
        table_name: &str,
        inline_pk_columns: &[String],
        table_constraints: &[spg_sql::ast::TableConstraint],
    ) -> Result<(), EngineError> {
        // v7.9.13 — implicit BTree per inline PK column +
        // v7.9.19 — implicit BTree on the leading column of every
        // table-level PRIMARY KEY / UNIQUE constraint.
        let table = self
            .active_catalog_mut()
            .get_mut(table_name)
            .expect("just created");
        let mut inline_lead_added: Option<alloc::string::String> = None;
        for (i, col_name) in inline_pk_columns.iter().enumerate() {
            let idx_name = if inline_pk_columns.len() == 1 {
                alloc::format!("{table_name}_pkey")
            } else {
                alloc::format!("{table_name}_pkey_{i}")
            };
            if let Err(e) = table.add_index(idx_name.clone(), col_name) {
                return Err(EngineError::Storage(e));
            }
            if i == 0 {
                inline_lead_added = Some(idx_name);
            }
        }
        // v7.38.1 (L12) — a multi-column PRIMARY KEY's leading index
        // becomes a REAL composite B-tree over the whole key, exactly
        // like PG's one `t_pkey` index. The k≥1 per-column B-trees
        // stay: they serve probes on non-leading columns, which a
        // composite cannot (a prefix must start at the front).
        if inline_pk_columns.len() >= 2
            && let Some(lead_name) = inline_lead_added
        {
            let mut extras: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
            for col_name in &inline_pk_columns[1..] {
                if let Some(p) = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col_name))
                {
                    extras.push(p);
                }
            }
            if extras.len() == inline_pk_columns.len() - 1 {
                if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == lead_name) {
                    idx.extra_column_positions = extras;
                }
                table
                    .convert_index_to_multi(&lead_name)
                    .map_err(EngineError::Storage)?;
            }
        }
        for (i, tc) in table_constraints.iter().enumerate() {
            // v7.17.0 Phase 2.2 — FULLTEXT KEY lands a real
            // tsvector-GIN per declared column instead of the
            // BTree the PK / UQ / KEY paths build. Branch early
            // so the BTree loop never sees the FULLTEXT shape.
            if let spg_sql::ast::TableConstraint::FulltextIndex { name, columns } = tc {
                for (k, col) in columns.iter().enumerate() {
                    let already = table.indices().iter().any(|idx| {
                        matches!(idx.kind, spg_storage::IndexKind::GinFulltext(_))
                            && table.schema().columns[idx.column_position].name == *col
                    });
                    if already {
                        continue;
                    }
                    let idx_name = match (name.as_ref(), columns.len(), k) {
                        (Some(n), 1, _) => n.clone(),
                        (Some(n), _, k) => alloc::format!("{n}_{k}"),
                        (None, _, _) => {
                            alloc::format!("{table_name}_{col}_ftidx")
                        }
                    };
                    if let Err(e) = table.add_gin_fulltext_index(idx_name, col) {
                        return Err(EngineError::Storage(e));
                    }
                }
                continue;
            }
            // v7.15.0 — plain KEY/INDEX rides this same loop so
            // the implicit BTree gets built. It carries its own
            // user-supplied name; PK/UQ still synthesise.
            let (suffix, names, explicit_name): (&str, &Vec<String>, Option<&String>) = match tc {
                spg_sql::ast::TableConstraint::PrimaryKey { columns, .. } => {
                    ("pkey", columns, None)
                }
                spg_sql::ast::TableConstraint::Unique { columns, .. } => ("key", columns, None),
                spg_sql::ast::TableConstraint::Index { name, columns } => {
                    ("idx", columns, name.as_ref())
                }
                spg_sql::ast::TableConstraint::Check { .. } => continue,
                // Handled by the early-branch above.
                spg_sql::ast::TableConstraint::FulltextIndex { .. } => continue,
                // v7.39 (round 210) — EXCLUDE builds no implicit index in
                // Phase 0 (O(n)-scan enforcement); a real GiST index is a
                // later perf phase.
                spg_sql::ast::TableConstraint::Exclude { .. } => continue,
            };
            // 7.38.1 S7 (tpcc decomposition finding) — a composite
            // PRIMARY KEY / UNIQUE built a BTree on the LEADING column
            // only, and TPC-C's keys all lead with the warehouse id:
            // at scale=1 every "index scan" selected the WHOLE table
            // (customer point lookup measured 19.9 ms over 30k rows).
            // SPG's BTree keys one column, so until composite-keyed
            // BTrees land (ledgered), the constraint builds one BTree
            // PER KEY COLUMN — the planner can then pick the selective
            // one (c_id: 10 rows) instead of the degenerate leading
            // one (c_w_id: all 30k). Mirrors what the inline-PK loop
            // above has always done.
            let mut lead_added: Option<alloc::string::String> = None;
            for (k, col_name) in names.iter().enumerate() {
                let already = table.indices().iter().any(|idx| {
                    matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                        && table.schema().columns[idx.column_position].name == *col_name
                });
                if already {
                    continue;
                }
                let idx_name = if let (Some(n), 0) = (explicit_name, k) {
                    n.clone()
                } else if names.len() == 1 {
                    alloc::format!("{table_name}_{col_name}_{suffix}")
                } else {
                    alloc::format!("{table_name}_{col_name}_{suffix}_{i}_{k}")
                };
                if let Err(e) = table.add_index(idx_name.clone(), col_name) {
                    return Err(EngineError::Storage(e));
                }
                if k == 0 {
                    lead_added = Some(idx_name);
                }
            }
            // v7.38.1 (L12) — same upgrade as the inline-PK path: the
            // leading index of a composite PK / UNIQUE / KEY becomes a
            // real multi-column B-tree over the whole declared tuple.
            if names.len() >= 2
                && let Some(lead_name) = lead_added
            {
                let mut extras: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                for col_name in &names[1..] {
                    if let Some(p) = table
                        .schema()
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    {
                        extras.push(p);
                    }
                }
                if extras.len() == names.len() - 1 {
                    if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == lead_name)
                    {
                        idx.extra_column_positions = extras;
                    }
                    table
                        .convert_index_to_multi(&lead_name)
                        .map_err(EngineError::Storage)?;
                }
            }
        }
        Ok(())
    }
}

impl Engine {
    /// v7.39 (RLS) — `CREATE POLICY`. Stores the policy on the table schema
    /// (independent of the RLS enable flag). Enforcement is Phase 1.
    pub(crate) fn exec_create_policy(
        &mut self,
        s: spg_sql::ast::CreatePolicyStatement,
    ) -> Result<QueryResult, EngineError> {
        let cmd = policy_cmd_to_storage(s.cmd);
        let using_expr = s.using.as_ref().map(deparse_policy_qual);
        let with_check_expr = s.with_check.as_ref().map(deparse_policy_qual);
        let table = self.active_catalog_mut().get_mut(&s.table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: s.table.clone(),
            })
        })?;
        if table.schema().policies.iter().any(|p| p.name == s.name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "policy {:?} for table {:?} already exists",
                s.name,
                s.table
            )));
        }
        table.schema_mut().policies.push(spg_storage::PolicyDef {
            name: s.name,
            cmd,
            permissive: s.permissive,
            roles: s.roles,
            using_expr,
            with_check_expr,
        });
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.39 (RLS) — `ALTER POLICY … { RENAME TO | [TO roles] [USING] [WITH
    /// CHECK] }`.
    pub(crate) fn exec_alter_policy(
        &mut self,
        s: spg_sql::ast::AlterPolicyStatement,
    ) -> Result<QueryResult, EngineError> {
        let new_using = s.using.as_ref().map(deparse_policy_qual);
        let new_check = s.with_check.as_ref().map(deparse_policy_qual);
        let table = self.active_catalog_mut().get_mut(&s.table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: s.table.clone(),
            })
        })?;
        // Duplicate-name pre-check for RENAME (before taking the mutable slot).
        if let Some(new) = &s.rename_to
            && table.schema().policies.iter().any(|p| &p.name == new)
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "policy {new:?} for table {:?} already exists",
                s.table
            )));
        }
        let pol = table
            .schema_mut()
            .policies
            .iter_mut()
            .find(|p| p.name == s.name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "policy {:?} for table {:?} does not exist",
                    s.name,
                    s.table
                ))
            })?;
        if let Some(new) = s.rename_to {
            pol.name = new;
        } else {
            if let Some(roles) = s.roles {
                pol.roles = roles;
            }
            if new_using.is_some() {
                pol.using_expr = new_using;
            }
            if new_check.is_some() {
                pol.with_check_expr = new_check;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.39 (RLS) — `DROP POLICY [IF EXISTS] name ON table`.
    pub(crate) fn exec_drop_policy(
        &mut self,
        s: spg_sql::ast::DropPolicyStatement,
    ) -> Result<QueryResult, EngineError> {
        let table = match self.active_catalog_mut().get_mut(&s.table) {
            Some(t) => t,
            None if s.if_exists => {
                return Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: self.catalog_change_is_committed(),
                });
            }
            None => {
                return Err(EngineError::Storage(StorageError::TableNotFound {
                    name: s.table.clone(),
                }));
            }
        };
        let before = table.schema().policies.len();
        table.schema_mut().policies.retain(|p| p.name != s.name);
        if table.schema().policies.len() == before && !s.if_exists {
            return Err(EngineError::Unsupported(alloc::format!(
                "policy {:?} for table {:?} does not exist",
                s.name,
                s.table
            )));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    pub(crate) fn exec_create_user(
        &mut self,
        s: &CreateUserStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.37 (round 828) — no transaction guard any more. PG treats
        // roles as ordinary catalog rows: BEGIN; CREATE ROLE r;
        // ROLLBACK leaves nothing, COMMIT publishes (measured against
        // PG18: count 0 after rollback, 1 after commit). The per-slot
        // guard that stood here since round 794 refused the statement
        // outright, which no drop-in client expects. Writes now go
        // through the TX role shadow (`role_ddl_users_mut`), so both
        // halves of PG's behaviour hold.
        let role = users::Role::parse(&s.role).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("invalid role: {:?}", s.role))
        })?;
        // Prefer the host-injected RNG. Falls back to a deterministic
        // salt derived from the username only when no RNG is wired —
        // acceptable for tests; the server always installs one.
        let salt = self.salt_fn.map_or_else(
            || {
                let mut s_bytes = [0u8; 16];
                let digest = spg_crypto::hash(s.name.as_bytes());
                s_bytes.copy_from_slice(&digest[..16]);
                s_bytes
            },
            |f| f(),
        );
        // v7.39 (TLS/SCRAM) — route through `create_user`, not `users.create`,
        // so the SQL path also derives the SCRAM-SHA-256 verifier. Without
        // this, a `CREATE USER … PASSWORD` user had `scram = None` and silently
        // fell back to cleartext pgwire auth.
        if self.effective_users().contains(&s.name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "role \"{}\" already exists",
                s.name
            )));
        }
        // v7.39 (read01 round 58) — a bare `CREATE ROLE devs` carries no
        // password. It cannot log in (NOLOGIN is its default), so it needs no
        // credential; give it an unguessable one derived from its own salt so
        // no code path ever sees an empty-password record.
        let password = if s.password.is_empty() {
            let digest = spg_crypto::hash(&salt);
            hex_of(&digest[..16])
        } else {
            s.password.clone()
        };
        self.create_user(&s.name, &password, role, salt)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE USER: {e}")))?;
        // PG's attribute defaults: LOGIN iff spelled CREATE USER, INHERIT, and
        // NOSUPERUSER — but SPG's own coarse `ROLE 'admin'` still means
        // superuser, which is how the existing admin account keeps working.
        // v7.39 (round 548) — remember whether a password was DECLARED,
        // not just whether the record ended up with one: the branch
        // above substitutes an unguessable credential for a bare
        // CREATE ROLE, and the wire's open-vs-authenticated decision
        // has to tell the two apart.
        self.role_ddl_users_mut()
            .set_password_declared(&s.name, !s.password.is_empty());
        self.role_ddl_users_mut().set_attributes(
            &s.name,
            s.login.unwrap_or(s.is_user),
            s.inherit.unwrap_or(true),
            s.superuser
                .unwrap_or_else(|| matches!(role, users::Role::Admin)),
        );
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_drop_user(
        &mut self,
        name: &str,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        // v7.37 (round 828) — transactional now; see exec_create_user.
        // v7.39 (read01 round 58) — PG's IF EXISTS skip NOTICE.
        if if_exists && !self.effective_users().contains(name) {
            self.notice(alloc::format!("role {name:?} does not exist, skipping"));
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        // v7.39 (read01 round 58) — PG refuses to drop a role that still holds
        // privileges: they would become dangling aclitems. It names the tables.
        let depends: alloc::vec::Vec<alloc::string::String> = self
            .active_catalog()
            .table_names()
            .into_iter()
            .filter(|t| {
                self.active_catalog().get(t).is_some_and(|tb| {
                    tb.schema()
                        .acl
                        .iter()
                        .any(|a| a.grantee.eq_ignore_ascii_case(name))
                        || tb
                            .schema()
                            .owner
                            .as_deref()
                            .is_some_and(|o| o.eq_ignore_ascii_case(name))
                })
            })
            .collect();
        if !depends.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "role \"{name}\" cannot be dropped because some objects depend on it DETAIL: privileges for table {}",
                depends.join(", ")
            )));
        }
        self.role_ddl_users_mut()
            .drop(name)
            .map_err(|e| EngineError::Unsupported(alloc::format!("DROP USER: {e}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    /// v7.12.4 — `CREATE [OR REPLACE] FUNCTION`. Stores the
    /// function metadata in the catalog. PL/pgSQL bodies are
    /// already parsed by the SQL parser; we re-canonicalise the
    /// body to source text for storage (the executor re-parses
    /// it at trigger fire time — see the trigger fire path).
    pub(crate) fn exec_create_function(
        &mut self,
        s: spg_sql::ast::CreateFunctionStatement,
    ) -> Result<QueryResult, EngineError> {
        let args_repr = render_function_args(&s.args);
        let returns = match &s.returns {
            spg_sql::ast::FunctionReturn::Trigger => alloc::string::String::from("TRIGGER"),
            spg_sql::ast::FunctionReturn::Void => alloc::string::String::from("VOID"),
            spg_sql::ast::FunctionReturn::Type(t) => alloc::format!("{t}"),
            spg_sql::ast::FunctionReturn::Other(s) => s.clone(),
        };
        let body_text = match &s.body {
            spg_sql::ast::FunctionBody::PlPgSql(b) => alloc::format!("{b}"),
            spg_sql::ast::FunctionBody::Raw(s) => s.clone(),
        };
        let def = spg_storage::FunctionDef {
            name: s.name.clone(),
            args_repr,
            returns,
            language: s.language.clone(),
            body: body_text,
            // v7.39 (read01 round 61) — whoever runs CREATE FUNCTION owns it.
            owner: Some(alloc::string::String::from(self.current_role())),
            acl: alloc::vec::Vec::new(),
            // v7.39 (round 322, V46) — the declared attribute clauses.
            volatility: match s.attrs.volatility {
                spg_sql::ast::FunctionVolatility::Immutable => spg_storage::FN_IMMUTABLE,
                spg_sql::ast::FunctionVolatility::Stable => spg_storage::FN_STABLE,
                spg_sql::ast::FunctionVolatility::Volatile => spg_storage::FN_VOLATILE,
            },
            strict: s.attrs.strict,
            security_definer: s.attrs.security_definer,
            leakproof: s.attrs.leakproof,
            parallel: match s.attrs.parallel {
                spg_sql::ast::FunctionParallel::Safe => spg_storage::FN_PARALLEL_SAFE,
                spg_sql::ast::FunctionParallel::Restricted => spg_storage::FN_PARALLEL_RESTRICTED,
                spg_sql::ast::FunctionParallel::Unsafe => spg_storage::FN_PARALLEL_UNSAFE,
            },
            cost: s.attrs.cost,
            rows: s.attrs.rows,
        };
        self.active_catalog_mut()
            .create_function(def, s.or_replace)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    /// v7.12.4 — `CREATE [OR REPLACE] TRIGGER`. The referenced
    /// function must already exist in the catalog (forward
    /// references defer to a later release). Persists the
    /// trigger metadata for the row-write hooks below to consult.
    pub(crate) fn exec_create_trigger(
        &mut self,
        s: spg_sql::ast::CreateTriggerStatement,
    ) -> Result<QueryResult, EngineError> {
        let timing = match s.timing {
            spg_sql::ast::TriggerTiming::Before => "BEFORE",
            spg_sql::ast::TriggerTiming::After => "AFTER",
            spg_sql::ast::TriggerTiming::InsteadOf => "INSTEAD OF",
        };
        let events: Vec<alloc::string::String> = s
            .events
            .iter()
            .map(|e| match e {
                spg_sql::ast::TriggerEvent::Insert => alloc::string::String::from("INSERT"),
                spg_sql::ast::TriggerEvent::Update => alloc::string::String::from("UPDATE"),
                spg_sql::ast::TriggerEvent::Delete => alloc::string::String::from("DELETE"),
                spg_sql::ast::TriggerEvent::Truncate => alloc::string::String::from("TRUNCATE"),
            })
            .collect();
        let for_each = match s.for_each {
            spg_sql::ast::TriggerForEach::Row => "ROW",
            spg_sql::ast::TriggerForEach::Statement => "STATEMENT",
        };
        // v7.39 (round 137) — INSTEAD OF triggers may only target views; BEFORE /
        // AFTER row triggers may only target base tables. PG's exact wording.
        let target_is_view = self.active_catalog().has_view(&s.table);
        if matches!(s.timing, spg_sql::ast::TriggerTiming::InsteadOf) {
            if !target_is_view {
                return Err(EngineError::Unsupported(alloc::format!(
                    "\"{}\" is a table DETAIL: Tables cannot have INSTEAD OF triggers.",
                    s.table
                )));
            }
            // v7.39 (round 137) — PG: INSTEAD OF triggers must be row-level.
            if matches!(s.for_each, spg_sql::ast::TriggerForEach::Statement) {
                return Err(EngineError::Unsupported(
                    "INSTEAD OF triggers must be FOR EACH ROW".into(),
                ));
            }
            // v7.39 (round 138) — PG: INSTEAD OF triggers cannot have WHEN.
            if s.when_condition.is_some() {
                return Err(EngineError::Unsupported(
                    "INSTEAD OF triggers cannot have WHEN conditions".into(),
                ));
            }
        } else if target_is_view {
            return Err(EngineError::Unsupported(alloc::format!(
                "\"{}\" is a view DETAIL: Views cannot have row-level BEFORE or AFTER triggers.",
                s.table
            )));
        }
        let def = spg_storage::TriggerDef {
            name: s.name.clone(),
            table: s.table.clone(),
            timing: alloc::string::String::from(timing),
            events,
            for_each: alloc::string::String::from(for_each),
            function: s.function.clone(),
            update_columns: s.update_columns.clone(),
            // v7.16.1 — every trigger is born enabled. Toggled
            // by ALTER TABLE … { ENABLE | DISABLE } TRIGGER.
            enabled: true,
            // v7.39 (round 138) — deparse the WHEN predicate to text; re-parsed
            // at fire time. Empty when there is no WHEN.
            when_condition: s
                .when_condition
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_default(),
        };
        self.active_catalog_mut()
            .create_trigger(def, s.or_replace)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_drop_trigger(
        &mut self,
        name: &str,
        table: &str,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let removed = self.active_catalog_mut().drop_trigger(name, table);
        if !removed && !if_exists {
            // v7.39 (round 700) — two fixes in one line, and they are the
            // same fix round 698 made for sequences.
            //
            // `StorageError::Corrupt` prefixes its Display with `corrupt
            // on-disk format: `, so a misspelt trigger name reported a
            // CORRUPTION to the client. And the wording was SPG's own
            // (`on "t"`); PG18 says `for table "t"`, which is what the
            // wire's classifier and any tool matching on it expect.
            //
            // Round 698 said its sweep found nothing else. It swept the
            // sequence / view / type shapes and not the trigger one — the
            // sweep was narrower than the sentence claimed.
            return Err(EngineError::Unsupported(alloc::format!(
                "trigger \"{name}\" for table \"{table}\" does not exist"
            )));
        }
        // v7.39 (round 282) — PG raises a NOTICE when IF EXISTS skips, and
        // it distinguishes the two ways a DROP TRIGGER can find nothing:
        // the RELATION is missing (so the trigger could not be looked up
        // at all), or the relation is there and the trigger is not.
        if !removed && if_exists {
            if self.active_catalog().get(table).is_none() {
                self.notice(alloc::format!(
                    "relation \"{table}\" does not exist, skipping"
                ));
            } else {
                self.notice(alloc::format!(
                    "trigger \"{name}\" for relation \"{table}\" does not exist, skipping"
                ));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    // v7.39 (round 139) — CREATE RULE (query-rewrite rules). Phase 1 supports
    // ON {INSERT|UPDATE|DELETE} TO table [WHERE cond] DO [ALSO|INSTEAD]
    // {NOTHING | command}. ON SELECT rules are PG's view mechanism; use CREATE
    // VIEW instead. The WHEN/commands are deparsed to text and re-parsed at DML
    // rewrite time, mirroring how triggers carry their WHEN predicate.
    pub(crate) fn exec_create_rule(
        &mut self,
        s: spg_sql::ast::CreateRuleStatement,
    ) -> Result<QueryResult, EngineError> {
        if s.event.eq_ignore_ascii_case("SELECT") {
            return Err(EngineError::Unsupported(
                "ON SELECT rules are not supported; use CREATE VIEW".into(),
            ));
        }
        // v7.39 (round 333, V59) — the conditional `DO INSTEAD <command>`
        // form is supported now: the rows the WHERE holds for take the
        // command, the rest run the original operation. It used to be
        // refused up front, which made a rule PG accepts a hard error.
        // Measured on PG 18.4: with `ON UPDATE TO r WHERE old.id > 1 DO
        // INSTEAD INSERT INTO log …`, `UPDATE r SET v = 999` answers
        // `UPDATE 1` — only the non-matching row is updated — and the
        // matching rows produce log entries instead.
        // Rules may target base tables (and, in PG, views); require the relation
        // to exist so a typo does not silently create a dead rule.
        let known = self.active_catalog().table_names().contains(&s.table)
            || self.active_catalog().has_view(&s.table);
        if !known {
            return Err(EngineError::Unsupported(alloc::format!(
                "relation \"{}\" does not exist",
                s.table
            )));
        }
        let def = spg_storage::RuleDef {
            name: s.name.clone(),
            table: s.table.clone(),
            event: s.event.to_ascii_uppercase(),
            instead: s.instead,
            when_condition: s
                .when_condition
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            commands: s.commands.iter().map(|c| c.to_string()).collect(),
        };
        self.active_catalog_mut()
            .create_rule(def, s.or_replace)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_drop_rule(
        &mut self,
        name: &str,
        table: &str,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let removed = self.active_catalog_mut().drop_rule(name, table);
        if !removed && !if_exists {
            // v7.39 (round 708) — PG's order and words, both measured: the
            // RELATION resolves first (`relation "t" does not exist`), and
            // only then the rule, spelled `for relation`, not `on`. The old
            // message also rode `StorageError::Corrupt`, whose Display put
            // `corrupt on-disk format:` in front of a typo — the same
            // wrapper rounds 698 and 700 kept meeting.
            if self.active_catalog().get(table).is_none() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation \"{table}\" does not exist"
                )));
            }
            return Err(EngineError::Unsupported(alloc::format!(
                "rule \"{name}\" for relation \"{table}\" does not exist"
            )));
        }
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    pub(crate) fn exec_drop_function(
        &mut self,
        name: &str,
        args: Option<&[alloc::string::String]>,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (read01 round 62) — with overloads, the signature says WHICH one.
        let removed = match args {
            Some(types) => {
                let repr = alloc::format!("({})", types.join(", "));
                let key = spg_storage::function_signature_key(name, &repr);
                self.active_catalog_mut().drop_function_by_key(&key)
            }
            None => {
                // PG refuses a bare `DROP FUNCTION f` when `f` is overloaded —
                // it cannot know which one is meant.
                if self.active_catalog().functions_named(name).len() > 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "function name \"{name}\" is not unique DETAIL: Specify the argument list to select the function unambiguously."
                    )));
                }
                self.active_catalog_mut().drop_function(name)
            }
        };
        if !removed && !if_exists {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("function {name:?} does not exist"),
            )));
        }
        // v7.39 (round 282) — the skipped-function NOTICE. Alone among the
        // IF EXISTS family PG does NOT quote the name, because it renders a
        // signature rather than an identifier.
        if !removed && if_exists {
            let sig = match args {
                Some(types) => types
                    .iter()
                    .map(|t| pg_signature_type_name(t))
                    .collect::<alloc::vec::Vec<_>>()
                    .join(","),
                None => alloc::string::String::new(),
            };
            self.notice(alloc::format!(
                "function {name}({sig}) does not exist, skipping"
            ));
        }
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    /// v7.17.0 — `CREATE SEQUENCE` engine path. Resolves
    /// `min_value` / `max_value` / `start` against PG defaults
    /// when omitted, then installs the SequenceDef in the catalog.
    pub(crate) fn exec_create_sequence(
        &mut self,
        s: spg_sql::ast::CreateSequenceStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 469) — a TEMPORARY sequence lives in the calling
        // session's namespace, exactly as round 436 put temporary tables
        // there. Until this round the keyword parsed and was dropped, so
        // the sequence was permanent: another connection saw it in
        // pg_class and could call nextval() on it. Measured against PG18,
        // where a second session sees nothing and errors on use.
        if s.temporary {
            let logical = s.name.clone();
            let mut inner = s;
            inner.temporary = false;
            inner.name = self.session_temp_name(&logical);
            let result = self.exec_create_sequence(inner)?;
            self.temp_sequences.insert(logical);
            self.refresh_temp_prefix();
            return Ok(result);
        }
        use spg_sql::ast::{SeqBound, SequenceDataType as AstDt};
        use spg_storage::{SequenceDataType, SequenceDef};
        let dt = match s.data_type {
            None => SequenceDataType::BigInt,
            Some(AstDt::SmallInt) => SequenceDataType::SmallInt,
            Some(AstDt::Int) => SequenceDataType::Int,
            Some(AstDt::BigInt) => SequenceDataType::BigInt,
        };
        let increment = s.options.increment.unwrap_or(1);
        if increment == 0 {
            return Err(EngineError::Unsupported(
                "INCREMENT must not be zero".into(),
            ));
        }
        let (def_min, def_max) = dt.default_bounds(increment > 0);
        let min_value = match s.options.min_value {
            None | Some(SeqBound::NoBound) => def_min,
            Some(SeqBound::Value(n)) => n,
        };
        let max_value = match s.options.max_value {
            None | Some(SeqBound::NoBound) => def_max,
            Some(SeqBound::Value(n)) => n,
        };
        if min_value > max_value {
            return Err(EngineError::Unsupported(alloc::format!(
                "MINVALUE ({min_value}) must be <= MAXVALUE ({max_value})"
            )));
        }
        let start = s
            .options
            .start
            .unwrap_or(if increment > 0 { min_value } else { max_value });
        // v7.39 (round 244) — PG splits the refusal into two named cases
        // (22023): below MINVALUE and above MAXVALUE.
        if start < min_value {
            return Err(EngineError::Unsupported(alloc::format!(
                "START value ({start}) cannot be less than MINVALUE ({min_value})"
            )));
        }
        if start > max_value {
            return Err(EngineError::Unsupported(alloc::format!(
                "START value ({start}) cannot be greater than MAXVALUE ({max_value})"
            )));
        }
        let cache = s.options.cache.unwrap_or(1);
        if cache < 1 {
            return Err(EngineError::Unsupported("CACHE must be >= 1".into()));
        }
        let cycle = s.options.cycle.unwrap_or(false);
        let owned_by = match s.options.owned_by {
            None | Some(spg_sql::ast::SequenceOwnedBy::None) => None,
            Some(spg_sql::ast::SequenceOwnedBy::Column { table, column }) => Some((table, column)),
        };
        let def = SequenceDef {
            name: s.name.clone(),
            data_type: dt,
            start,
            increment,
            min_value,
            max_value,
            cache,
            cycle,
            owned_by,
            last_value: start,
            is_called: false,
            // v7.39 (read01 round 60) — whoever runs CREATE SEQUENCE owns it.
            owner: Some(alloc::string::String::from(self.current_role())),
            acl: alloc::vec::Vec::new(),
        };
        // v7.39 (read01 round 46) — PG's IF NOT EXISTS skip NOTICE. The
        // storage call swallows the collision when the flag is set, so
        // detect it here before handing over.
        if s.if_not_exists && self.active_catalog().has_sequence(&s.name) {
            self.notice(alloc::format!(
                "relation {:?} already exists, skipping",
                s.name
            ));
        }
        self.active_catalog_mut()
            .create_sequence(def, s.if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 — `ALTER SEQUENCE` engine path. Re-uses the catalog
    /// `alter_sequence` merge helper.
    pub(crate) fn exec_alter_sequence(
        &mut self,
        s: spg_sql::ast::AlterSequenceStatement,
    ) -> Result<QueryResult, EngineError> {
        use spg_sql::ast::SeqBound;
        // v7.29 (round-23a) - implicit serial sequences materialise
        // on first address, ALTER SEQUENCE included.
        self.ensure_implicit_sequence(&s.name);
        // v7.39 (read01 round 49) — RENAME TO is its own form, not an option.
        if let Some(new) = s.rename_to {
            self.active_catalog_mut()
                .rename_sequence(&s.name, &new)
                .map_err(EngineError::Storage)?;
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: self.catalog_change_is_committed(),
            });
        }
        let cat = self.active_catalog_mut();
        if !cat.has_sequence(&s.name) {
            if s.if_exists {
                return Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                });
            }
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("sequence {:?} does not exist", s.name),
            )));
        }
        let min_value = match s.options.min_value {
            None => None,
            Some(SeqBound::NoBound) => None, // NO MINVALUE → keep current
            Some(SeqBound::Value(n)) => Some(n),
        };
        let max_value = match s.options.max_value {
            None => None,
            Some(SeqBound::NoBound) => None,
            Some(SeqBound::Value(n)) => Some(n),
        };
        let owned_by = s.options.owned_by.map(|ob| match ob {
            spg_sql::ast::SequenceOwnedBy::None => None,
            spg_sql::ast::SequenceOwnedBy::Column { table, column } => Some((table, column)),
        });
        cat.alter_sequence(
            &s.name,
            s.options.increment,
            min_value,
            max_value,
            s.options.start,
            s.options.restart,
            s.options.cache,
            s.options.cycle,
            owned_by,
        )
        .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.2 — `CREATE VIEW` engine path. Stores the
    /// Display-rendered body verbatim in the catalog; SELECT-from-
    /// view at exec time re-parses + prepends as a synthetic CTE.
    pub(crate) fn exec_create_view(
        &mut self,
        s: spg_sql::ast::CreateViewStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 469) — same as the temporary sequence above: the
        // keyword parsed and was dropped, so the view was permanent and
        // every other connection could select from it.
        if s.temporary {
            let logical = s.name.clone();
            let mut inner = s;
            inner.temporary = false;
            inner.name = self.session_temp_name(&logical);
            let result = self.exec_create_view(inner)?;
            self.temp_views.insert(logical);
            self.refresh_temp_prefix();
            return Ok(result);
        }
        // v7.39 (round 151) — PG rejects data-modifying CTEs in a view
        // body (DefineView, view.c): the definition would run the write
        // on every reference. Read-only WITH is fine.
        if s.body.ctes.iter().any(|c| c.body.is_modifying()) {
            return Err(EngineError::Unsupported(
                "views must not contain data-modifying statements in WITH".into(),
            ));
        }
        // v7.39 (read01 round 81) — CREATE OR REPLACE VIEW may only APPEND
        // columns; PG forbids renaming, dropping, reordering or retyping an
        // existing column ("cannot change name of view column …", "cannot drop
        // columns from view", "cannot change data type of view column …"). SPG
        // let every one of these through and silently swapped the view's shape,
        // so a downstream `SELECT known_col FROM v` would start resolving to a
        // different column, or vanish — data corruption disguised as a DDL.
        if s.or_replace && self.active_catalog().has_view(&s.name) {
            self.check_view_replace_columns(&s)?;
        }
        // v7.39 (round 700) — the BODY has to resolve. PG analyses a view
        // definition at CREATE time, so `CREATE VIEW v AS SELECT * FROM
        // nosuch` is `relation "nosuch" does not exist`. SPG stored it and
        // reported success, leaving a view that appears in `pg_views`, that
        // every SELECT against fails, and that a dump then carries forward
        // — a broken object made by a statement that said it worked.
        //
        // The probe is `view_output_columns`, which the OR REPLACE path
        // already runs: a `LIMIT 0` execution of the same body. It resolves
        // relations and columns without producing rows, so the check costs
        // one empty plan and cannot disagree with what the view will do,
        // because it IS what the view will do.
        self.view_output_columns(&s.body, &s.columns)?;
        // Render the SELECT body to canonical form so the catalog
        // round-trips a deterministic source (no whitespace /
        // comment surprises in the on-disk snapshot).
        let columns = s.columns.clone();
        let name = s.name.clone();
        let or_replace = s.or_replace;
        let if_not_exists = s.if_not_exists;
        // v7.39 (round 132) — persist WITH CHECK OPTION as a u8 (0/1/2).
        let check_option = match s.check_option {
            None => 0,
            Some(spg_sql::ast::ViewCheckOption::Local) => 1,
            Some(spg_sql::ast::ViewCheckOption::Cascaded) => 2,
        };
        let body_repr = alloc::format!("{}", spg_sql::ast::Statement::Select(s.body));
        let def = spg_storage::ViewDef {
            name,
            columns,
            body: body_repr,
            check_option,
        };
        self.active_catalog_mut()
            .create_view(def, or_replace, if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// The (name, type) of each column a view body produces. Runs the body
    /// through the real executor with a zero-row bound, so it reflects exactly
    /// what a SELECT from the view would return — column overrides, view-on-view
    /// expansion, joins and all. Types come from the empty result's schema.
    pub(crate) fn view_output_columns(
        &self,
        body: &spg_sql::ast::SelectStatement,
        overrides: &[String],
    ) -> Result<alloc::vec::Vec<(String, spg_storage::DataType)>, EngineError> {
        let mut probe = body.clone();
        probe.limit = Some(spg_sql::ast::LimitExpr::Literal(0));
        let QueryResult::Rows { mut columns, .. } =
            self.exec_select_cancel(&probe, crate::CancelToken::none())?
        else {
            return Err(EngineError::Unsupported(
                "view body must be a row-returning SELECT".into(),
            ));
        };
        for (i, ov) in overrides.iter().enumerate() {
            if let Some(c) = columns.get_mut(i) {
                c.name = ov.clone();
            }
        }
        Ok(columns.into_iter().map(|c| (c.name, c.ty)).collect())
    }

    /// PG's CREATE OR REPLACE VIEW column rule: the new column list must be the
    /// old one, optionally with columns appended. Same names, same order, same
    /// types for every pre-existing position.
    fn check_view_replace_columns(
        &self,
        s: &spg_sql::ast::CreateViewStatement,
    ) -> Result<(), EngineError> {
        let old_def = self.active_catalog().view(&s.name).cloned();
        let Some(old_def) = old_def else {
            return Ok(());
        };
        let old_body = match spg_sql::parser::parse_statement(&old_def.body) {
            Ok(spg_sql::ast::Statement::Select(b)) => b,
            // A body we can no longer parse is not something to block a replace
            // on — let the replace proceed rather than wedge the view.
            _ => return Ok(()),
        };
        let old_cols = self.view_output_columns(&old_body, &old_def.columns)?;
        let new_cols = self.view_output_columns(&s.body, &s.columns)?;
        if new_cols.len() < old_cols.len() {
            return Err(EngineError::Unsupported(
                "cannot drop columns from view".into(),
            ));
        }
        for (old, new) in old_cols.iter().zip(new_cols.iter()) {
            if old.0 != new.0 {
                return Err(EngineError::Unsupported(alloc::format!(
                    "cannot change name of view column \"{}\" to \"{}\"",
                    old.0,
                    new.0
                )));
            }
            if old.1 != new.1 {
                return Err(EngineError::Unsupported(alloc::format!(
                    "cannot change data type of view column \"{}\" from {} to {}",
                    old.0,
                    crate::system_catalog::pg_data_type_text(old.1),
                    crate::system_catalog::pg_data_type_text(new.1),
                )));
            }
        }
        Ok(())
    }

    /// v7.17.0 Phase 1.4 — `CREATE TYPE name AS ENUM (…)` engine
    /// path. Registers the enum in the catalog with order-
    /// preserving labels. PG semantics: CREATE TYPE errors if the
    /// name is taken (no IF NOT EXISTS).
    pub(crate) fn exec_create_type(
        &mut self,
        s: spg_sql::ast::CreateTypeStatement,
    ) -> Result<QueryResult, EngineError> {
        // Name-collision check against tables / sequences / views /
        // materialized views.
        let cat = self.active_catalog();
        if cat.get(&s.name).is_some() {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} would shadow an existing table", s.name),
            )));
        }
        if cat.has_sequence(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} would shadow an existing sequence", s.name),
            )));
        }
        if cat.has_view(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} would shadow an existing view", s.name),
            )));
        }
        // v7.37.42-T2 ζ-B — pre-check collision with the
        // composite registry too, so creating ENUM with a name
        // already used by a composite (or vice versa) fails
        // uniformly regardless of which kind comes first.
        if cat.composite_types().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} already exists", s.name),
            )));
        }
        if cat.enum_types().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} already exists", s.name),
            )));
        }
        if cat.domain_types().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} already exists", s.name),
            )));
        }
        // v7.37.42-T2 ζ-B — composite types now live in their own
        // catalog registry (composite_types), parallel to enum_types
        // / domain_types. ENUM stays in enum_types as before.
        match s.kind {
            spg_sql::ast::TypeKind::Enum { labels } => {
                if labels.is_empty() {
                    return Err(EngineError::Unsupported(
                        "CREATE TYPE … AS ENUM requires at least one label".into(),
                    ));
                }
                // Reject duplicate labels per PG.
                for i in 0..labels.len() {
                    for j in (i + 1)..labels.len() {
                        if labels[i] == labels[j] {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "CREATE TYPE {:?}: duplicate ENUM label {:?}",
                                s.name,
                                labels[i]
                            )));
                        }
                    }
                }
                let def = spg_storage::EnumDef {
                    name: s.name.clone(),
                    labels,
                };
                self.active_catalog_mut()
                    .create_enum_type(def)
                    .map_err(EngineError::Storage)?;
            }
            spg_sql::ast::TypeKind::Composite {
                fields,
                field_user_types,
            } => {
                // v7.39 (round 769, F31 tranche 5 #140) — an attribute-less
                // composite is legal PG (`CREATE TYPE x AS ()`, measured); the
                // old engine-side guard doubled the parser's former refusal.
                // Reject duplicate field names per PG.
                for i in 0..fields.len() {
                    for j in (i + 1)..fields.len() {
                        if fields[i].0.eq_ignore_ascii_case(&fields[j].0) {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "CREATE TYPE {:?}: duplicate composite field {:?}",
                                s.name,
                                fields[i].0
                            )));
                        }
                    }
                }
                // Resolve each field's ColumnTypeName → DataType.
                let resolved_fields = fields
                    .into_iter()
                    .map(|(fname, fty)| (fname, column_type_to_data_type(fty)))
                    .collect::<alloc::vec::Vec<_>>();
                // v7.39 (round 264) — a field naming another COMPOSITE keeps
                // that name; the engine resolves the inner record through it.
                let cat = self.active_catalog();
                let field_user_types: alloc::vec::Vec<Option<alloc::string::String>> =
                    field_user_types
                        .into_iter()
                        .map(|n| n.filter(|n| cat.composite_types().contains_key(n)))
                        .collect();
                let def = spg_storage::CompositeDef {
                    name: s.name.clone(),
                    fields: resolved_fields,
                    field_user_types,
                };
                self.active_catalog_mut()
                    .create_composite_type(def)
                    .map_err(EngineError::Storage)?;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }
    /// v7.39 (round 260) — `ALTER DOMAIN`. Every form used to be
    /// swallowed by the parser's pg_dump no-op arm: success reported,
    /// nothing changed. Constraint names and the error wordings are PG's,
    /// probed live.
    pub(crate) fn exec_alter_domain(
        &mut self,
        name: &str,
        action: spg_sql::ast::AlterDomainAction,
    ) -> Result<QueryResult, EngineError> {
        use spg_sql::ast::AlterDomainAction as A;
        let not_found = || {
            EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                "type {name:?} does not exist"
            )))
        };
        if !self.active_catalog().domain_types().contains_key(name) {
            return Err(not_found());
        }
        match action {
            A::AddConstraint { name: cname, check } => {
                let dom = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?;
                // PG's auto-name for an unnamed ALTER-added check follows
                // the same `<domain>_check{n}` sequence as CREATE DOMAIN.
                let cname = match cname {
                    Some(c) => c,
                    None => {
                        let mut i = dom.checks.len();
                        loop {
                            let cand = if i == 0 {
                                alloc::format!("{name}_check")
                            } else {
                                alloc::format!("{name}_check{i}")
                            };
                            if !dom.checks.iter().any(|c| c.name == cand) {
                                break cand;
                            }
                            i += 1;
                        }
                    }
                };
                if dom.checks.iter().any(|c| c.name == cname) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "constraint \"{cname}\" for domain \"{name}\" already exists"
                    )));
                }
                let expr = alloc::format!("{check}");
                let mut def = dom.clone();
                def.checks
                    .push(spg_storage::DomainCheck { name: cname, expr });
                self.replace_domain(name, def)?;
            }
            A::DropConstraint {
                name: cname,
                if_exists,
            } => {
                let mut def = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?
                    .clone();
                let before = def.checks.len();
                def.checks.retain(|c| c.name != cname);
                if def.checks.len() == before {
                    if if_exists {
                        return Ok(QueryResult::CommandOk {
                            affected: 0,
                            modified_catalog: false,
                        });
                    }
                    return Err(EngineError::Unsupported(alloc::format!(
                        "constraint \"{cname}\" of domain \"{name}\" does not exist"
                    )));
                }
                self.replace_domain(name, def)?;
            }
            A::SetDefault(e) => {
                let mut def = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?
                    .clone();
                def.default = Some(alloc::format!("{e}"));
                self.replace_domain(name, def)?;
            }
            A::DropDefault => {
                let mut def = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?
                    .clone();
                def.default = None;
                self.replace_domain(name, def)?;
            }
            A::SetNotNull | A::DropNotNull => {
                // v7.39 (round 260) — SET NOT NULL must reject when an
                // existing column of this domain already holds NULLs (PG:
                // `column "v" of table "adt" contains null values`).
                if matches!(action, A::SetNotNull) {
                    let snap = self.current_snapshot();
                    let cat = self.active_catalog();
                    let mut offender: Option<(alloc::string::String, alloc::string::String)> = None;
                    'outer: for tname in cat.table_names() {
                        let Some(table) = cat.get(&tname) else {
                            continue;
                        };
                        let cols = table.schema().columns.clone();
                        let idxs: alloc::vec::Vec<usize> = cols
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| c.user_domain_type.as_deref() == Some(name))
                            .map(|(i, _)| i)
                            .collect();
                        if idxs.is_empty() {
                            continue;
                        }
                        for (_, row) in table.scan_visible(&snap) {
                            for &i in &idxs {
                                if row.values.get(i).is_none_or(spg_storage::Value::is_null) {
                                    offender = Some((tname.clone(), cols[i].name.clone()));
                                    break 'outer;
                                }
                            }
                        }
                    }
                    if let Some((t, c)) = offender {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{c}\" of table \"{t}\" contains null values"
                        )));
                    }
                }
                let mut def = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?
                    .clone();
                def.nullable = matches!(action, A::DropNotNull);
                self.replace_domain(name, def)?;
            }
            A::RenameTo(new_name) => {
                if self.active_catalog().domain_types().contains_key(&new_name) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "type {new_name:?} already exists"
                    )));
                }
                let mut def = self
                    .active_catalog()
                    .domain_types()
                    .get(name)
                    .ok_or_else(not_found)?
                    .clone();
                def.name = new_name.clone();
                self.active_catalog_mut().drop_domain_type(name);
                self.active_catalog_mut()
                    .create_domain_type(def)
                    .map_err(EngineError::Storage)?;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.39 (round 260) — swap a domain definition in place.
    fn replace_domain(
        &mut self,
        name: &str,
        def: spg_storage::DomainDef,
    ) -> Result<(), EngineError> {
        self.active_catalog_mut().drop_domain_type(name);
        self.active_catalog_mut()
            .create_domain_type(def)
            .map_err(EngineError::Storage)
    }

    /// v7.17.0 Phase 1.5 — `CREATE DOMAIN name AS base [DEFAULT
    /// expr] [NOT NULL] [CHECK (expr)]*` engine path. Stores the
    /// base type + Display-rendered CHECK / DEFAULT sources so
    /// INSERT/UPDATE on bound columns can re-eval the checks.
    pub(crate) fn exec_create_domain(
        &mut self,
        s: spg_sql::ast::CreateDomainStatement,
    ) -> Result<QueryResult, EngineError> {
        let cat = self.active_catalog();
        if cat.domain_types().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("domain {:?} already exists", s.name),
            )));
        }
        if cat.get(&s.name).is_some()
            || cat.has_sequence(&s.name)
            || cat.has_view(&s.name)
            || cat.enum_types().contains_key(&s.name)
        {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("domain {:?} would shadow an existing object", s.name),
            )));
        }
        // v7.39 (round 259) — `CREATE DOMAIN child AS parent`: the parent
        // supplies the ultimate scalar type (the parser typed the unknown
        // name as Text), and its NAME is recorded so the check walk can
        // reach the parent's constraints — which an ALTER on the parent
        // must keep affecting, so the chain is walked at check time rather
        // than copied here (probed against PG).
        let mut base_domain: Option<alloc::string::String> = None;
        let mut base_type = column_type_to_data_type(s.base_type);
        if let Some(parent) = &s.base_domain {
            if let Some(pd) = cat.domain_types().get(parent) {
                base_type = pd.base_type;
                base_domain = Some(parent.clone());
            } else if !cat.enum_types().contains_key(parent) {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("type {parent:?} does not exist"),
                )));
            }
        }
        let default = s.default.as_ref().map(|e| alloc::format!("{e}"));
        // v7.39 (round 260) — PG names an unnamed domain CHECK
        // `<domain>_check`, then `_check1`, `_check2`, … (probed).
        let checks = s
            .checks
            .iter()
            .enumerate()
            .map(|(i, e)| spg_storage::DomainCheck {
                name: if i == 0 {
                    alloc::format!("{}_check", s.name)
                } else {
                    alloc::format!("{}_check{i}", s.name)
                },
                expr: alloc::format!("{e}"),
            })
            .collect::<Vec<_>>();
        let def = spg_storage::DomainDef {
            name: s.name.clone(),
            base_type,
            nullable: !s.not_null,
            default,
            checks,
            base_domain,
        };
        self.active_catalog_mut()
            .create_domain_type(def)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.5 — `DROP DOMAIN [IF EXISTS] names`.
    pub(crate) fn exec_drop_domain(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            let was_present = self.active_catalog_mut().drop_domain_type(name);
            if was_present {
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("domain {name:?} does not exist"),
                )));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.6 — `CREATE SCHEMA [IF NOT EXISTS] name`.
    /// Registers the schema in the catalog. Schema-qualified
    /// table references continue to strip the prefix at lookup
    /// time (prefix routing, not isolation — see project-next-
    /// docket for the v7.18+ real-isolation tracking).
    pub(crate) fn exec_create_schema(
        &mut self,
        name: String,
        if_not_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (read01 round 46) — PG's IF NOT EXISTS skip NOTICE.
        if if_not_exists && self.active_catalog().schema_exists(&name) {
            self.notice(alloc::format!("schema {name:?} already exists, skipping"));
        }
        self.active_catalog_mut()
            .create_schema(name, if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.6 — `DROP SCHEMA [IF EXISTS] names`.
    /// Built-in schemas always reject the drop with a clear
    /// error.
    pub(crate) fn exec_drop_schema(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            let was_present = self
                .active_catalog_mut()
                .drop_schema(name)
                .map_err(EngineError::Storage)?;
            if was_present {
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("schema {name:?} does not exist"),
                )));
            } else {
                // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                self.notice(alloc::format!("schema {name:?} does not exist, skipping"));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.4 — `DROP TYPE [IF EXISTS] names`. Only
    /// ENUM types are catalogued today; other types silently
    /// no-op even outside IF EXISTS to mirror the prior
    /// "everything's text" lax stance.
    pub(crate) fn exec_drop_type(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            // v7.37.42-T2 ζ-B — DROP TYPE searches ENUM + COMPOSITE
            // registries (PG groups CREATE TYPE … AS ENUM and
            // CREATE TYPE … AS (…) under the same DROP TYPE
            // command).
            let cat = self.active_catalog_mut();
            let was_enum = cat.drop_enum_type(name);
            let was_composite = cat.drop_composite_type(name);
            if was_enum || was_composite {
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("type {name:?} does not exist"),
                )));
            } else {
                // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                self.notice(alloc::format!("type {name:?} does not exist, skipping"));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.3 — `CREATE MATERIALIZED VIEW` engine path.
    /// Materialises the body at CREATE time (unless WITH NO DATA),
    /// stores the result as a regular `Table`, and registers the
    /// body source in the catalog so REFRESH can re-run it.
    pub(crate) fn exec_create_materialized_view(
        &mut self,
        s: spg_sql::ast::CreateMaterializedViewStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 436) — `CREATE TEMPORARY TABLE x AS <select>` arrives
        // here (CTAS lowers to this node with `as_plain_table`). Same
        // treatment as the column-list form: build it under the session's
        // namespace prefix and remember it there.
        if s.temporary && s.as_plain_table {
            let logical = s.name.clone();
            let mut inner = s;
            inner.temporary = false;
            inner.name = self.session_temp_name(&logical);
            let result = self.exec_create_materialized_view(inner)?;
            self.temp_tables.insert(logical);
            self.refresh_temp_prefix();
            return Ok(result);
        }
        // v7.39 (round 151) — PG's matview wording differs from the
        // plain-view one (transformCreateTableAsStmt, analyze.c).
        if s.body.ctes.iter().any(|c| c.body.is_modifying()) {
            return Err(EngineError::Unsupported(
                "materialized views must not use data-modifying statements in WITH".into(),
            ));
        }
        // Name-collision check (table / view / sequence / mat-view).
        let cat = self.active_catalog();
        if cat.materialized_views().contains_key(&s.name) || cat.get(&s.name).is_some() {
            if s.if_not_exists {
                return Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                });
            }
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("materialized view {:?} already exists", s.name),
            )));
        }
        if cat.has_view(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!(
                    "materialized view {:?} would shadow an existing view",
                    s.name
                ),
            )));
        }
        if cat.has_sequence(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!(
                    "materialized view {:?} would shadow an existing sequence",
                    s.name
                ),
            )));
        }
        // Render the body to canonical form for the registry.
        let body_repr = alloc::format!("{}", spg_sql::ast::Statement::Select(s.body.clone()));
        // Execute the body to learn the columns. With WITH DATA we
        // also materialise the rows; with WITH NO DATA we only need
        // the schema, so re-use a LIMIT 0 wrap to keep the column
        // inference path uniform without paying for the rows.
        let result = self.exec_select_cancel(&s.body, CancelToken::none())?;
        let (mut cols, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            other => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CREATE MATERIALIZED VIEW body did not return rows: {other:?}"
                )));
            }
        };
        // Apply the column-rename list per PG semantics.
        if !s.columns.is_empty() {
            if s.columns.len() != cols.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CREATE MATERIALIZED VIEW {:?}: column list has {} names but body returns {}",
                    s.name,
                    s.columns.len(),
                    cols.len()
                )));
            }
            for (c, name) in cols.iter_mut().zip(s.columns.iter()) {
                c.name.clone_from(name);
            }
        }
        // Promote any synthetic-Text projections to their actual
        // observed types so the backing table accepts the rows.
        cols = infer_column_types(&cols, &rows);
        let schema = spg_storage::TableSchema::new(s.name.clone(), cols);
        let cat = self.active_catalog_mut();
        cat.create_table(schema).map_err(EngineError::Storage)?;
        // v7.38.19 — the materialised row count is the statement's
        // answer, not a detail. PG tags CTAS and CREATE MATERIALIZED
        // VIEW `SELECT <n>`, and a driver reads that to learn how many
        // rows it wrote. Returning 0 here made every CTAS report writing
        // nothing while writing the right rows -- silent, and the wrong
        // half is the one a program acts on.
        let mut materialised = 0usize;
        if s.with_data {
            let table = cat
                .get_mut(&s.name)
                .expect("just-created materialized-view backing table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
                materialised += 1;
            }
        }
        // v7.38 (read01 P6.49) — CTAS / SELECT INTO produce a plain table; only
        // a real MATERIALIZED VIEW gets a registry entry (and REFRESH support).
        if !s.as_plain_table {
            cat.register_materialized_view(s.name.clone(), body_repr);
            // v7.39 (round 737, S14/B3) — register for delta maintenance
            // when the body qualifies; the fan-out starts buffering from
            // the next statement on.
            if let Some(base) = matview_maintainable_base(&s.body) {
                self.matview_maintainable.insert(s.name.clone(), base);
            }
        }
        Ok(QueryResult::CommandOk {
            affected: materialised,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.3 — `REFRESH MATERIALIZED VIEW name [WITH
    /// [NO] DATA]`. Looks up the source, re-runs it, replaces the
    /// backing table's rows.
    pub(crate) fn exec_refresh_materialized_view(
        &mut self,
        name: &str,
        with_data: bool,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 699) — PG18 distinguishes the two ways this fails,
        // and SPG gave one sentence for both:
        //
        //   missing name        `relation "x" does not exist`
        //   exists, wrong kind  `"x" is not a materialized view`
        //
        // The second is the one that matters to a caller: it says the name
        // resolved and the OBJECT is not what the statement is for, which
        // is a different thing to go and check.
        //
        // Both were `StorageError::Corrupt`, the same wrapper round 698
        // found putting `corrupt on-disk format:` in front of a plain typo.
        // `Unsupported` carries no banner, and the wire's classifier reads
        // `relation "…" does not exist` for 42P01 already.
        let source = match self
            .active_catalog()
            .materialized_views()
            .get(name)
            .cloned()
        {
            Some(s) => s,
            None => {
                let exists = self.active_catalog().get(name).is_some();
                return Err(EngineError::Unsupported(if exists {
                    alloc::format!("\"{name}\" is not a materialized view")
                } else {
                    alloc::format!("relation \"{name}\" does not exist")
                }));
            }
        };
        let parsed = spg_sql::parser::parse_statement(&source).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "materialized view {name:?} body re-parse failed: {e}"
            ))
        })?;
        let Statement::Select(body) = parsed else {
            return Err(EngineError::Unsupported(alloc::format!(
                "materialized view {name:?} body is not a SELECT (catalog corruption)"
            )));
        };
        // v7.39 (round 735, S14/B3) — the refresh watermark. When the
        // body's FULL dependency set is provable (plain stored tables
        // only — any CTE / union / subquery / expression source makes
        // the collector answer None) and no dependency's change
        // sequence moved since the last refresh, this REFRESH is an
        // O(1) no-op with an identical observable result. PG recomputes
        // unconditionally — this is the incremental-maintenance first
        // step its architecture doesn't have. WITH NO DATA never
        // no-ops (its contract is to EMPTY the view).
        let deps = if with_data {
            matview_dep_tables(&body)
        } else {
            None
        };
        if let Some(dep_tables) = &deps {
            let current: alloc::vec::Vec<(String, u64)> = dep_tables
                .iter()
                .map(|t| {
                    (
                        t.clone(),
                        self.table_change_seq.get(t.as_str()).copied().unwrap_or(0),
                    )
                })
                .collect();
            if self
                .matview_refresh_watermark
                .get(name)
                .is_some_and(|last| *last == current)
            {
                return Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                });
            }
            // v7.39 (round 737, S14/B3 knife 2) — INSERT-ONLY delta
            // application. The base changed; if this view is registered
            // maintainable, has a watermark (i.e. its buffer covers
            // everything since the last full refresh), did not
            // overflow, and every buffered change is an Insert, the new
            // rows run through the projection and APPEND — no truncate,
            // no rescan. Any delete / update / tombstone in the buffer
            // falls back to the full path this round (their row-map
            // machinery is the next knife). Either way the watermark
            // and buffer reset below.
            if with_data
                && self.matview_maintainable.contains_key(name)
                && self.matview_refresh_watermark.contains_key(name)
                && !self.matview_delta_overflow.contains(name)
                && self
                    .matview_delta_buf
                    .get(name)
                    .is_some_and(|b| !b.is_empty())
            {
                let buf = self.matview_delta_buf.remove(name).expect("checked above");
                // v7.39 (round 738) — ordered application: Insert /
                // Delete / Tombstone in ARRIVAL order (an insert later
                // deleted must land then leave). None = this buffer
                // cannot be applied (an Update, or no row map where one
                // is needed) -> the full path below.
                let outcome = self.apply_matview_delta_ordered(name, &body, &buf)?;
                if outcome.is_some() {
                    crate::MATVIEW_DELTA_APPLIED
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                } else {
                    crate::MATVIEW_DELTA_BAILED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                if let Some(applied) = outcome {
                    let current: alloc::vec::Vec<(String, u64)> = dep_tables
                        .iter()
                        .map(|t| {
                            (
                                t.clone(),
                                self.table_change_seq.get(t.as_str()).copied().unwrap_or(0),
                            )
                        })
                        .collect();
                    self.matview_refresh_watermark
                        .insert(String::from(name), current);
                    return Ok(QueryResult::CommandOk {
                        affected: applied,
                        modified_catalog: self.catalog_change_is_committed(),
                    });
                }
            }
        }
        // Wipe the existing rows first (PG truncates the matview
        // and rebuilds; we approximate with an empty INSERT loop).
        {
            let cat = self.active_catalog_mut();
            let table = cat.get_mut(name).ok_or_else(|| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                    "materialized view {name:?} backing table missing"
                )))
            })?;
            table.truncate();
        }
        if !with_data {
            self.matview_refresh_watermark.remove(name);
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: self.catalog_change_is_committed(),
            });
        }
        // v7.39 (round 738, S14/B3 knife 3) — a maintainable view's FULL
        // refresh scans the base table internally instead of running the
        // body SQL: same rows (single stored table, pure projection,
        // pure WHERE — that is what registration means), but each output
        // row's base RowId is in hand, which is the only place the
        // delete/tombstone row map can be built. Non-maintainable views
        // keep the SQL path and carry no map.
        let internal = if let Some(base) = matview_maintainable_base(&body) {
            let snap = self.current_snapshot();
            let t = self.active_catalog().get(&base).ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "materialized view {name:?} base table {base:?} missing"
                ))
            })?;
            let base_cols = t.schema().columns.clone();
            let alias = body
                .from
                .as_ref()
                .and_then(|f| f.primary.alias.clone())
                .unwrap_or_else(|| base.clone());
            let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
            let mut pairs: alloc::vec::Vec<(u64, spg_storage::Row<'static>)> =
                alloc::vec::Vec::new();
            let t = self.active_catalog().get(&base).expect("checked above");
            for (i, row) in t.rows().iter().enumerate() {
                if !t.is_row_visible(i, &snap) {
                    continue;
                }
                if let Some(w) = &body.where_ {
                    let cond = eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                    if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                        continue;
                    }
                }
                let mut vals = alloc::vec::Vec::with_capacity(body.items.len());
                for item in &body.items {
                    let spg_sql::ast::SelectItem::Expr { expr, .. } = item else {
                        unreachable!("maintainable admits Expr items only");
                    };
                    vals.push(eval::eval_expr(expr, row, &ctx).map_err(EngineError::Eval)?);
                }
                let rid = t
                    .rowids()
                    .get(i)
                    .copied()
                    .unwrap_or(spg_storage::row_header::RowId::UNASSIGNED);
                pairs.push((rid.0, spg_storage::Row::new(vals)));
            }
            Some(pairs)
        } else {
            None
        };
        if let Some(pairs) = internal {
            let cat = self.active_catalog_mut();
            let table = cat.get_mut(name).expect("backing table verified above");
            let mut map: alloc::collections::BTreeMap<u64, usize> =
                alloc::collections::BTreeMap::new();
            let affected = pairs.len();
            for (rid, row) in pairs {
                table.insert(row).map_err(EngineError::Storage)?;
                map.insert(rid, table.rows().len() - 1);
            }
            let expected = table.rows().len();
            self.matview_row_map
                .insert(String::from(name), (expected, map));
            if let Some(dep_tables) = deps {
                let current: alloc::vec::Vec<(String, u64)> = dep_tables
                    .iter()
                    .map(|t| {
                        (
                            t.clone(),
                            self.table_change_seq.get(t.as_str()).copied().unwrap_or(0),
                        )
                    })
                    .collect();
                self.matview_refresh_watermark
                    .insert(String::from(name), current);
            }
            self.matview_delta_buf.remove(name);
            self.matview_delta_overflow.remove(name);
            if let Some(base) = matview_maintainable_base(&body) {
                self.matview_maintainable.insert(String::from(name), base);
            }
            return Ok(QueryResult::CommandOk {
                affected,
                modified_catalog: self.catalog_change_is_committed(),
            });
        }
        self.matview_row_map.remove(name);
        let rows = match self.exec_select_cancel(&body, CancelToken::none())? {
            QueryResult::Rows { rows, .. } => rows,
            other => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "REFRESH MATERIALIZED VIEW {name:?} body did not return rows: {other:?}"
                )));
            }
        };
        let cat = self.active_catalog_mut();
        let table = cat.get_mut(name).expect("backing table verified above");
        let affected = rows.len();
        for row in rows {
            table.insert(row).map_err(EngineError::Storage)?;
        }
        // v7.39 (round 735, S14/B3) — record what this full refresh saw.
        // Re-read the sequences AFTER the recompute: a write that landed
        // mid-refresh moves a seq past what we record only if it came
        // first (single-writer engine), so recording the pre-read values
        // could mask it; the post-read cannot.
        if let Some(dep_tables) = deps {
            let current: alloc::vec::Vec<(String, u64)> = dep_tables
                .iter()
                .map(|t| {
                    (
                        t.clone(),
                        self.table_change_seq.get(t.as_str()).copied().unwrap_or(0),
                    )
                })
                .collect();
            self.matview_refresh_watermark
                .insert(String::from(name), current);
        }
        // v7.39 (round 737) — a full refresh resets the delta machinery:
        // stale buffered changes are superseded, overflow clears, and
        // (re)registration keeps a view maintainable across restarts,
        // where CREATE never re-runs.
        self.matview_delta_buf.remove(name);
        self.matview_delta_overflow.remove(name);
        if let Some(base) = matview_maintainable_base(&body) {
            self.matview_maintainable.insert(String::from(name), base);
        } else {
            self.matview_maintainable.remove(name);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.3 — `DROP MATERIALIZED VIEW [IF EXISTS]
    /// names`. Drops the backing table + unregisters the source.
    pub(crate) fn exec_drop_materialized_view(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            let was_present = self
                .active_catalog_mut()
                .drop_materialized_view_source(name);
            if was_present {
                // Drop the backing table too.
                self.active_catalog_mut().drop_table(name);
                // v7.39 (round 737, S14/B3) — retire every maintenance
                // structure with the view.
                self.matview_maintainable.remove(name);
                self.matview_delta_buf.remove(name);
                self.matview_delta_overflow.remove(name);
                self.matview_refresh_watermark.remove(name);
                self.matview_row_map.remove(name);
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("materialized view {name:?} does not exist"),
                )));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 Phase 1.2 — `DROP VIEW [IF EXISTS] name [, name…]`.
    pub(crate) fn exec_drop_view(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            // v7.39 (round 469) — a bare DROP names the session's
            // temporary view first, the way `Catalog::drop_table` resolves
            // a temporary table.
            let key = self.active_catalog().view_key(name);
            let was_present = self.active_catalog_mut().drop_view(&key);
            if was_present && key != *name {
                self.temp_views.remove(name);
                self.refresh_temp_prefix();
            }
            if !was_present {
                if !if_exists {
                    // v7.39 (read01 round 89) — PG's 42P01 wording, without the
                    // "corrupt on-disk format:" prefix a Storage::Corrupt adds.
                    return Err(EngineError::Unsupported(alloc::format!(
                        "view \"{name}\" does not exist"
                    )));
                }
                // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                self.notice(alloc::format!("view {name:?} does not exist, skipping"));
            }
            if was_present {
                removed += 1;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }

    /// v7.17.0 — `DROP SEQUENCE [IF EXISTS] name [, name…]`.
    pub(crate) fn exec_drop_sequence(
        &mut self,
        names: &[String],
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let mut removed = 0usize;
        for name in names {
            let key = self.active_catalog().sequence_key(name);
            let was_present = self.active_catalog_mut().drop_sequence(&key);
            if was_present && key != *name {
                self.temp_sequences.remove(name);
                self.refresh_temp_prefix();
            }
            if !was_present {
                if !if_exists {
                    return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                        alloc::format!("sequence {name:?} does not exist"),
                    )));
                }
                // v7.39 (read01 round 46) — PG's IF EXISTS skip NOTICE.
                self.notice(alloc::format!("sequence {name:?} does not exist, skipping"));
            }
            if was_present {
                removed += 1;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && self.catalog_change_is_committed(),
        })
    }
}

// ---- column-definition / DEFAULT / SET / enum helpers (lib.rs split 11) ----

/// v7.9.21 — resolve a column's DEFAULT for INSERT-time
/// default-fill. Free fn (rather than `&self`) so callers
/// with an active `&mut Table` borrow can still use it.
/// Literal defaults take the cached path (`col.default`);
/// runtime defaults hit `clock_fn` at each call. mailrs G4.
/// v7.39 (read01 round 93) — truncate a generated identifier to PG's
/// NAMEDATALEN-1 (63) byte limit, on a UTF-8 char boundary so a
/// multi-byte name is never split mid-codepoint.
fn truncate_ident(name: &mut String) {
    const MAX: usize = 63;
    if name.len() <= MAX {
        return;
    }
    let mut cut = MAX;
    while cut > 0 && !name.is_char_boundary(cut) {
        cut -= 1;
    }
    name.truncate(cut);
}

pub(crate) fn resolve_column_default_free(
    col: &ColumnSchema,
    clock_fn: Option<ClockFn>,
    // v7.39 (round 525) — the session, for a DEFAULT that names one.
    sess: Option<&crate::eval::DmlSession>,
) -> Result<Value<'static>, EngineError> {
    if let Some(rt) = &col.runtime_default {
        return eval_runtime_default_free(rt, col.ty, clock_fn, sess);
    }
    Ok(col.default.clone().unwrap_or(Value::Null))
}

pub(crate) fn eval_runtime_default_free(
    rt: &str,
    ty: DataType,
    clock_fn: Option<ClockFn>,
    sess: Option<&crate::eval::DmlSession>,
) -> Result<Value<'static>, EngineError> {
    let s = rt.trim().to_ascii_lowercase();
    // v7.17.0 Phase 2.1 — also strip `(N)` precision suffix
    // so MySQL `CURRENT_TIMESTAMP(6)` resolves the same as
    // bare `CURRENT_TIMESTAMP`. SPG stores TIMESTAMP at fixed
    // microsecond resolution; the precision modifier is
    // parser-only.
    let with_no_parens = s.trim_end_matches("()");
    let canonical: &str = if let Some(open_idx) = with_no_parens.find('(') {
        if with_no_parens.ends_with(')') {
            &with_no_parens[..open_idx]
        } else {
            with_no_parens
        }
    } else {
        with_no_parens
    };
    let now_us = match clock_fn {
        Some(f) => f(),
        None => 0,
    };
    let v = match canonical {
        "now" | "current_timestamp" | "localtimestamp" => Value::Timestamp(now_us),
        "current_date" => Value::Date((now_us / 86_400_000_000) as i32),
        "current_time" | "localtime" => Value::Timestamp(now_us),
        // v7.17.0 — UUID generators in DEFAULT clauses. Required
        // for the canonical Django / Rails / Hibernate `id UUID
        // PRIMARY KEY DEFAULT gen_random_uuid()` pattern. Each
        // INSERT evaluates the function fresh; the per-row UUID
        // is the storage value, not a cached literal.
        "gen_random_uuid" | "uuid_generate_v4" => Value::Uuid(eval::gen_random_uuid_bytes()),
        // v7.39 (round 525) — anything else is EVALUATED, not refused.
        // PG takes any expression as a DEFAULT; the eight names above are
        // a fast path that skips a parse per row, and this was the whole
        // list SPG accepted — `DEFAULT current_setting('app.tenant')`,
        // `DEFAULT upper(…)`, `DEFAULT 2 * 3` all failed the INSERT.
        _ => {
            let expr = spg_sql::parser::parse_expression(rt).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "runtime DEFAULT expression {rt:?} does not parse: {e}"
                ))
            })?;
            let no_cols: [ColumnSchema; 0] = [];
            let mut ctx = eval::EvalContext::new(&no_cols, None);
            if let Some(sv) = sess {
                ctx = ctx.with_session(sv);
            }
            let row = spg_storage::Row::new(alloc::vec::Vec::new());
            let v = eval::eval_expr(&expr, &row, &ctx).map_err(|e| EngineError::Eval(e))?;
            return coerce_value(v, ty, "DEFAULT", 0);
        }
    };
    coerce_value(v, ty, "DEFAULT", 0)
}

/// v7.9.21 — true when a DEFAULT expression needs INSERT-time
/// evaluation rather than being cacheable as a literal Value.
/// FunctionCall is the immediate case (`now()`,
/// `current_timestamp`). Literal expressions and simple sign-
/// flipped numerics still take the static-cache path.
/// v7.39 (RLS) — translate the parser's `PolicyCmd` to the storage one.
fn policy_cmd_to_storage(c: spg_sql::ast::PolicyCmd) -> spg_storage::PolicyCmd {
    use spg_sql::ast::PolicyCmd as A;
    use spg_storage::PolicyCmd as S;
    match c {
        A::All => S::All,
        A::Select => S::Select,
        A::Insert => S::Insert,
        A::Update => S::Update,
        A::Delete => S::Delete,
    }
}

/// v7.38.19 — a DEFAULT that is a call to `nextval`, however it spells
/// its argument. `nextval('s')` and `nextval('s'::regclass)` are the
/// same column; `pg_dump` writes the second.
fn is_nextval_call(e: &Expr) -> bool {
    matches!(e, Expr::FunctionCall { name, args }
        if name.eq_ignore_ascii_case("nextval") && args.len() == 1)
}

fn is_runtime_default_expr(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { .. } => true,
        Expr::Unary { expr, .. } => is_runtime_default_expr(expr),
        _ => false,
    }
}

/// v7.38 (read01) — PG's canonical parenless deparse spelling for the SQL-
/// standard niladic keyword functions. The parser lowers `CURRENT_DATE` &c
/// to a synthetic `FunctionCall { name: "current_date", args: [] }`; PG's
/// `pg_get_expr` renders these as the bare uppercase keyword (not
/// `current_date()`), so a default that uses one must deparse the same way.
/// Returns `None` for a real function (`now()`) which keeps its call form.
fn pg_parenless_keyword(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "current_date" => Some("CURRENT_DATE"),
        "current_time" => Some("CURRENT_TIME"),
        "current_timestamp" => Some("CURRENT_TIMESTAMP"),
        "localtime" => Some("LOCALTIME"),
        "localtimestamp" => Some("LOCALTIMESTAMP"),
        "current_user" => Some("CURRENT_USER"),
        "session_user" => Some("SESSION_USER"),
        "current_role" => Some("CURRENT_ROLE"),
        "current_catalog" => Some("CURRENT_CATALOG"),
        _ => None,
    }
}

/// v7.38 (read01) — deparse a column DEFAULT expression to the PG-compatible
/// source text cached on `ColumnSchema.default_text` (surfaced by
/// information_schema.columns.column_default / pg_attrdef / pg_get_expr).
///
/// SPG's `Expr` Display already matches PG's deparse for non-negative integer
/// / numeric / boolean literals, arithmetic (`(3 + 4)`), and ordinary function
/// calls (`now()`). This additionally matches PG for the shapes where Display
/// diverges: bare string literals (PG types them, `'hi'::text`), the parenless
/// SQL-standard keyword functions (`CURRENT_DATE`, not `current_date()`), and
/// negative numeric constants, which PG's `get_const_expr` folds into a typed
/// literal (`int DEFAULT -5` → `'-5'::integer`, `numeric DEFAULT -1.5` →
/// `'-1.5'::numeric`).
///
/// KNOWN Phase-2 residuals (fall through to Display, a valid but not
/// byte-identical-to-PG spelling — documented in the read01 checklist):
///   * integer literals wider than int4 (`bigint DEFAULT 5000000000` →
///     PG `'5000000000'::bigint`; SPG `5000000000`);
///   * string / numeric literals nested inside a larger expression, which PG
///     types per operand (`'hi' || 'there'` → PG `('hi'::text ||
///     'there'::text)`). Full parity needs PG's recursive `get_rule_expr`
///     constant-typing deparser.
fn deparse_default(expr: &Expr, col_ty: DataType) -> alloc::string::String {
    match expr {
        // Bare string literal → PG's typed-literal form `'…'::<coltype>`.
        // 7.38.1 S5.2 — the typed-literal cast must name the SQL type
        // (`text[]`), not information_schema's category word (`ARRAY`):
        // pg_dump copies this text into the dumped DEFAULT, and
        // `'{}'::ARRAY` parses nowhere — not even back into SPG.
        Expr::Literal(Literal::String(s)) => alloc::format!(
            "'{}'::{}",
            s.replace('\'', "''"),
            crate::conversions::pg_type_name_for_error(col_ty)
        ),
        // r1054 — an ALREADY-typed string literal re-parses as a Cast
        // node, and the generic Display arm below rendered it
        // `('dflt')::text` where the first pass wrote `'dflt'::text`:
        // two producers of default_text, two spellings, and the dump
        // round-trip stopped being a fixed point on exactly that line.
        // Same normalized shape as the bare-literal arm (PG stores a
        // default through the assignment cast and reports the column's
        // type, so re-normalizing to `col_ty` matches PG here too).
        Expr::Cast { expr: inner, .. }
            if matches!(inner.as_ref(), Expr::Literal(Literal::String(_))) =>
        {
            let Expr::Literal(Literal::String(s)) = inner.as_ref() else {
                unreachable!("guarded by matches!")
            };
            alloc::format!(
                "'{}'::{}",
                s.replace('\'', "''"),
                crate::conversions::pg_type_name_for_error(col_ty)
            )
        }
        // v7.38.19 — a call renders its arguments the way PostgreSQL
        // prints them, which for a typed string literal is
        // `'zs'::regclass` and not `('zs')::regclass`.
        //
        // The generic Display arm below parenthesises a Cast, so
        // `nextval('zs'::regclass)` — what `pg_dump` writes for a serial
        // column, and what a schema-diff tool compares — read back as
        // `nextval(('zs')::regclass)`. It re-parses here and the dump
        // round-trip is a fixed point, so this never broke anything of
        // ours; it broke the comparison with theirs, which is the bar.
        //
        // r1054 fixed the same spelling for a default that IS a cast.
        // This is the same fix one level in.
        // Narrow on purpose: only a call that CARRIES such an argument
        // is re-rendered. Taking every call broke `CURRENT_DATE`, which
        // the parser lowers to a zero-argument `current_date` whose
        // Display prints the keyword — this arm printed the lowering.
        // The existing default-text test caught it in the same minute.
        Expr::FunctionCall { name, args }
            if args.iter().any(|a| {
                matches!(a, Expr::Cast { expr: inner, .. }
                    if matches!(inner.as_ref(), Expr::Literal(Literal::String(_))))
            }) =>
        {
            let rendered: Vec<alloc::string::String> = args
                .iter()
                .map(|a| match a {
                    Expr::Cast {
                        expr: inner,
                        target,
                    } if matches!(inner.as_ref(), Expr::Literal(Literal::String(_))) => {
                        let Expr::Literal(Literal::String(lit)) = inner.as_ref() else {
                            unreachable!("guarded by matches!")
                        };
                        alloc::format!("'{}'::{target}", lit.replace('\'', "''"))
                    }
                    other => alloc::format!("{other}"),
                })
                .collect();
            alloc::format!("{name}({})", rendered.join(", "))
        }
        // Boolean literal → PG's lowercase `true` / `false` (SPG's Literal
        // Display emits uppercase `TRUE`).
        Expr::Literal(Literal::Bool(b)) => {
            alloc::string::String::from(if *b { "true" } else { "false" })
        }
        // Negative numeric constant: PG folds `- <lit>` into a typed Const.
        // The cast type is the *literal's* natural type (integer / numeric),
        // not the column type.
        Expr::Unary {
            op: spg_sql::ast::UnOp::Neg,
            expr: inner,
        } => match inner.as_ref() {
            Expr::Literal(Literal::Integer(n)) => alloc::format!("'-{n}'::integer"),
            Expr::Literal(Literal::Float(_) | Literal::NumericBig(_) | Literal::Numeric { .. }) => {
                alloc::format!("'-{inner}'::numeric")
            }
            _ => alloc::format!("{expr}"),
        },
        // Parenless SQL-standard keyword functions → bare uppercase keyword.
        Expr::FunctionCall { name, args } if args.is_empty() => {
            if let Some(kw) = pg_parenless_keyword(name) {
                alloc::string::String::from(kw)
            } else {
                alloc::format!("{expr}")
            }
        }
        _ => alloc::format!("{expr}"),
    }
}

/// v7.39 (RLS) — deparse a policy `USING` / `WITH CHECK` qual to PG-compatible
/// text for pg_policy / pg_policies / pg_dump. SPG's `Expr` Display already
/// matches PG for column comparisons and operators; this recursively rewrites
/// the niladic SQL-standard keyword functions a policy qual commonly uses
/// (`current_user` → `CURRENT_USER`, &c) which Display would render as
/// `current_user()`. The stored form re-parses identically, so enforcement is
/// unaffected. (String-literal `::text` typing is the shared default_text
/// Phase-2 residual and is left to Display.)
pub(crate) fn deparse_policy_qual(e: &Expr) -> alloc::string::String {
    match e {
        Expr::FunctionCall { name, args } if args.is_empty() => pg_parenless_keyword(name)
            .map_or_else(|| alloc::format!("{e}"), alloc::string::String::from),
        Expr::Binary { lhs, op, rhs } => alloc::format!(
            "({} {op} {})",
            deparse_policy_qual(lhs),
            deparse_policy_qual(rhs)
        ),
        Expr::Unary { op, expr } => {
            use spg_sql::ast::UnOp;
            let inner = deparse_policy_qual(expr);
            match op {
                UnOp::Not => alloc::format!("(NOT {inner})"),
                UnOp::Neg => alloc::format!("(-{inner})"),
                UnOp::Plus => alloc::format!("(+{inner})"),
                UnOp::BitNot => alloc::format!("(~{inner})"),
            }
        }
        Expr::Cast { expr, target } => {
            alloc::format!("({}::{target})", deparse_policy_qual(expr))
        }
        Expr::IsNull { expr, negated } => {
            let inner = deparse_policy_qual(expr);
            if *negated {
                alloc::format!("({inner} IS NOT NULL)")
            } else {
                alloc::format!("({inner} IS NULL)")
            }
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let op = match (negated, case_insensitive) {
                (false, false) => "LIKE",
                (true, false) => "NOT LIKE",
                (false, true) => "ILIKE",
                (true, true) => "NOT ILIKE",
            };
            alloc::format!(
                "({} {op} {})",
                deparse_policy_qual(expr),
                deparse_policy_qual(pattern)
            )
        }
        Expr::FunctionCall { name, args } => {
            let rendered: alloc::vec::Vec<_> = args.iter().map(deparse_policy_qual).collect();
            alloc::format!("{name}({})", rendered.join(", "))
        }
        _ => alloc::format!("{e}"),
    }
}

/// v7.17.0 Phase 1.4 — INSERT/UPDATE-time enum label check. When
/// `col_idx` has a registered label list, the cell value must be
/// NULL or one of the labels (case-sensitive per PG).
/// v7.17.0 Phase 3.P0-37 — validate + canonicalise a MySQL inline
/// SET cell. For non-SET columns this is a no-op pass-through.
///
/// Semantics:
///   * NULL preserved.
///   * Empty string → `''` (zero flags).
///   * Otherwise split on ',', trim each token, validate every
///     token against the column's variant list (error on miss),
///     de-dup, then re-emit in DEFINITION order joined by ','.
pub(crate) fn canonicalize_set_value(
    lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
    col_idx: usize,
    col_name: &str,
    value: Value<'static>,
) -> Result<Value<'static>, EngineError> {
    let Some(variants) = lookup.get(&col_idx) else {
        return Ok(value);
    };
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => {
            if s.is_empty() {
                return Ok(Value::text(alloc::string::String::new()));
            }
            // Collect a presence-set of variant indices to keep
            // definition order + handle de-dup in one pass.
            let mut present = alloc::vec![false; variants.len()];
            for raw in s.split(',') {
                let tok = raw.trim();
                if tok.is_empty() {
                    continue;
                }
                let idx = variants.iter().position(|v| v == tok).ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!(
                        "column {col_name:?}: invalid SET token {tok:?}; \
                         allowed: {variants:?}"
                    ))
                })?;
                present[idx] = true;
            }
            // Re-emit in definition order.
            let mut out = alloc::string::String::new();
            let mut first = true;
            for (i, keep) in present.iter().enumerate() {
                if !keep {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&variants[i]);
            }
            Ok(Value::text(out))
        }
        other => Err(EngineError::Unsupported(alloc::format!(
            "column {col_name:?}: SET-typed column expects TEXT, got {}",
            crate::conversions::pg_type_name_for_error_opt(other.data_type())
        ))),
    }
}

pub(crate) fn enforce_enum_label(
    lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
    col_idx: usize,
    col_name: &str,
    value: &Value,
) -> Result<(), EngineError> {
    if let Some(labels) = lookup.get(&col_idx) {
        match value {
            Value::Null => Ok(()),
            Value::Text(s) => {
                if labels.iter().any(|l| l == s) {
                    Ok(())
                } else {
                    Err(EngineError::Unsupported(alloc::format!(
                        "column {col_name:?}: invalid enum label {s:?}; allowed: {labels:?}"
                    )))
                }
            }
            other => Err(EngineError::Unsupported(alloc::format!(
                "column {col_name:?}: enum-typed column expects TEXT, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ))),
        }
    } else {
        Ok(())
    }
}

fn column_def_to_schema(c: ColumnDef, mysql: bool) -> Result<ColumnSchema, EngineError> {
    let ty = column_type_to_data_type(c.ty);
    let mut schema = ColumnSchema::new(c.name.clone(), ty, c.nullable);
    // user_type_ref is the raw ident the parser couldn't resolve
    // to a built-in; classification into enum vs domain happens
    // at exec_create_table where we have catalog access. We
    // park it temporarily as user_enum_type and the engine
    // promotes domain bindings to user_domain_type before the
    // table is stored.
    if let Some(name) = c.user_type_ref {
        schema.user_enum_type = Some(name);
    }
    // v7.17.0 Phase 2.1 — render the ON UPDATE expression to
    // canonical text (the engine re-parses at UPDATE time).
    if let Some(expr) = c.on_update_runtime {
        schema.on_update_runtime = Some(alloc::format!("{expr}"));
    }
    // v7.17.0 Phase 2.5 — bridge the AST `Collation` enum to the
    // storage one. Same variants, different crates (spg-storage
    // owns no dep on spg-sql).
    // v7.39 (round 370, M4 P4a) — under the MySQL dialect a TEXT column
    // with NO explicit `COLLATE` takes the folding default collation
    // (utf8mb4_uca1400_ai_ci), so it stores CaseInsensitive and the
    // read/write paths fold it. An explicit `COLLATE utf8mb4_bin` keeps
    // Binary (byte-wise) — both resolve to AST `Binary`, so the explicit
    // flag is what tells them apart.
    let is_text_col = matches!(
        ty,
        spg_storage::DataType::Text
            | spg_storage::DataType::Varchar(_)
            | spg_storage::DataType::Char(_)
    );
    // v7.39 (round 676) — carry the collation NAME as written, which
    // `Collation` below cannot: it folds C / POSIX / en_US / default into
    // one value. `pg_attribute.attcollation` reads this to answer 950 for a
    // column declared `COLLATE "C"` instead of the type's default 100.
    schema.collation_name = c.collation_name.clone();
    schema.collation = if mysql && is_text_col && !c.collation_explicit {
        spg_storage::Collation::CaseInsensitive
    } else {
        match c.collation {
            spg_sql::ast::Collation::Binary => spg_storage::Collation::Binary,
            spg_sql::ast::Collation::CaseInsensitive => spg_storage::Collation::CaseInsensitive,
        }
    };
    // v7.17.0 Phase 4.4 — MySQL `UNSIGNED` flag propagates to
    // storage so engine INSERT / UPDATE can range-check.
    schema.is_unsigned = c.is_unsigned;
    // v7.39 (round 386, type-fidelity epic P1) — declared TINYINT /
    // MEDIUMINT width, lost when the type collapsed to SmallInt / Int.
    // Drives the epic-P2 write-path range check.
    schema.mysql_int_width = c.mysql_int_width.map(|w| match w {
        spg_sql::ast::MysqlIntWidth::Tiny => spg_storage::MysqlIntWidth::Tiny,
        spg_sql::ast::MysqlIntWidth::Medium => spg_storage::MysqlIntWidth::Medium,
        spg_sql::ast::MysqlIntWidth::Small => spg_storage::MysqlIntWidth::Small,
        spg_sql::ast::MysqlIntWidth::Int => spg_storage::MysqlIntWidth::Int,
        spg_sql::ast::MysqlIntWidth::Big => spg_storage::MysqlIntWidth::Big,
    });
    // v7.39 (round 424, type-fidelity epic) — declared fractional-seconds
    // precision of a MySQL temporal column. Drives write-path truncation
    // and render padding; None keeps PG's full-microsecond behaviour.
    schema.mysql_fsp = c.mysql_fsp;
    // v7.39 (round 389, type-fidelity epic P4a) — a "real" SMALLINT /
    // INT UNSIGNED holds a range its signed storage tag cannot (65535 /
    // 4294967295), so widen the storage one step and record the declared
    // width for the range check + dump rendering. The `is_none()` guard
    // skips TINYINT UNSIGNED (i16 already holds 0..255) and MEDIUMINT
    // UNSIGNED (i32 already holds 0..16777215) — they keep their tag.
    if schema.is_unsigned && schema.mysql_int_width.is_none() {
        match schema.ty {
            spg_storage::DataType::SmallInt => {
                schema.ty = spg_storage::DataType::Int;
                schema.mysql_int_width = Some(spg_storage::MysqlIntWidth::Small);
            }
            spg_storage::DataType::Int => {
                schema.ty = spg_storage::DataType::BigInt;
                schema.mysql_int_width = Some(spg_storage::MysqlIntWidth::Int);
            }
            // v7.39 (round 471, epic P4b) — BIGINT UNSIGNED reaches
            // 18446744073709551615, which i64 cannot hold at all: SPG used
            // to REFUSE anything past 2^63-1 with `expected BIGINT, got
            // NUMERIC(0)`, so a MariaDB table with a real u64 in it could
            // not be loaded. Numeric is i128-backed with scale 0 and
            // already compares, orders, indexes and renders as an exact
            // integer; the width marker keeps the declared type for
            // SHOW CREATE and information_schema.
            spg_storage::DataType::BigInt => {
                schema.ty = spg_storage::DataType::Numeric {
                    precision: 20,
                    scale: 0,
                };
                schema.mysql_int_width = Some(spg_storage::MysqlIntWidth::Big);
            }
            _ => {}
        }
    }
    // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM variant list.
    // INSERT validation lives in coerce_value (Text → Text path
    // with the column's variant list as the accept-set).
    schema.inline_enum_variants = c.inline_enum_variants;
    // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant list.
    // INSERT canonicalisation (de-dup + sort by definition order)
    // lives in the exec_insert path next to the ENUM check.
    schema.inline_set_variants = c.inline_set_variants;
    // v7.37.7(sentori Epic 3 P1)— stored generated-column
    // expression. Carry the Display-form source to storage; the
    // engine re-parses and re-evaluates on every INSERT / UPDATE.
    if let Some(gen_expr) = c.generated_stored_expr {
        schema.generated_stored_expr = Some(alloc::format!("{gen_expr}"));
    }
    // v7.38 (read01) — GENERATED ALWAYS AS IDENTITY marker. The engine
    // rejects an explicit non-DEFAULT INSERT value for such a column
    // unless the statement carries OVERRIDING SYSTEM VALUE.
    schema.identity_always = c.identity_always;
    if let Some(default_expr) = c.default {
        // v7.38 (read01) — cache the PG-compatible source text of the DEFAULT
        // expression for catalog introspection, independent of the
        // literal/runtime split below (which loses the source spelling).
        schema.default_text = Some(deparse_default(&default_expr, ty));
        // v7.9.21 — distinguish literal defaults (evaluated once
        // at CREATE TABLE) from expression defaults (deferred to
        // INSERT). Function calls (`now()`, `current_timestamp`
        // — see v7.9.20 keyword promotion) take the runtime path.
        // Literals continue to cache. mailrs G4.
        // v7.38.19 — a `nextval(…)` DEFAULT is the column being
        // NUMBERED, not an expression to re-evaluate per row.
        //
        // Advancing a sequence needs a mutable catalog, and the context a
        // runtime DEFAULT is evaluated in does not hold one -- so this
        // stored the call as text and every INSERT that left the column
        // to its default answered `nextval() requires a sequence
        // resolver (read-only context)`. PostgreSQL 18.4 inserts.
        //
        // The OTHER spelling of the same column has worked since v7.22:
        // `ALTER TABLE … SET DEFAULT nextval(…)` lowers to the
        // auto-increment marker, because that is what `pg_dump` emits
        // for a serial column and imports were losing their numbering.
        // Two spellings of one column definition disagreed about whether
        // the column worked at all. This is the same lowering, reached
        // from the other side.
        if is_nextval_call(&default_expr) {
            if !matches!(ty, DataType::SmallInt | DataType::Int | DataType::BigInt) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "auto-increment applies to integer columns only ({:?} is {ty:?})",
                    c.name
                )));
            }
            schema.auto_increment = true;
        } else if is_runtime_default_expr(&default_expr) {
            let display = alloc::format!("{default_expr}");
            schema = schema.with_runtime_default(display);
        } else {
            let raw = literal_expr_to_value(default_expr)?;
            // v7.39 (round 259) — a column whose type is a user type is
            // still typed with the parser's Text placeholder here; the
            // real type only arrives when the domain binding is resolved
            // (exec_create_table). Coercing now made `w wd DEFAULT 7`
            // fail outright — a hard error on valid SQL — so the domain
            // case keeps the raw value and is coerced there instead.
            let coerced = if schema.user_enum_type.is_some() {
                raw
            } else {
                coerce_value(raw, ty, &c.name, 0)?
            };
            schema = schema.with_default(coerced);
        }
    }
    if c.auto_increment {
        // AUTO_INCREMENT only makes sense on integer-shaped columns.
        if !matches!(ty, DataType::SmallInt | DataType::Int | DataType::BigInt) {
            return Err(EngineError::Unsupported(alloc::format!(
                "AUTO_INCREMENT requires an integer column type, got {ty:?}"
            )));
        }
        schema = schema.with_auto_increment();
    }
    Ok(schema)
}

/// v7.12.4 — render a function arg list into the
/// canonical form the storage layer caches as
/// [`spg_storage::FunctionDef::args_repr`]. The catalogue uses
/// this string for both display + as a coarse signature key
/// for the (deferred) overload resolution v7.12.5+ adds.
fn render_function_args(args: &[spg_sql::ast::FunctionArg]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::from("(");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match a.mode {
            spg_sql::ast::FunctionArgMode::In => {}
            spg_sql::ast::FunctionArgMode::Out => out.push_str("OUT "),
            spg_sql::ast::FunctionArgMode::InOut => out.push_str("INOUT "),
        }
        if let Some(n) = &a.name {
            out.push_str(n);
            out.push(' ');
        }
        match &a.ty {
            spg_sql::ast::FunctionArgType::Typed(t) => {
                let _ = write!(out, "{t}");
            }
            spg_sql::ast::FunctionArgType::Raw(s) => out.push_str(s),
        }
    }
    out.push(')');
    out
}

/// v7.39 (read01 round 48) — is `name` already taken by a constraint on this
/// table? Checks the stored names of foreign keys, uniqueness constraints and
/// CHECKs. Constraints written before FILE_VERSION 60 have no stored name, so
/// they can't collide here — they are still reachable by their synthesised
/// name through `resolve_constraint`.
fn constraint_name_taken(table: &spg_storage::Table, name: &str) -> bool {
    let sch = table.schema();
    sch.foreign_keys
        .iter()
        .any(|f| f.name.as_deref() == Some(name))
        || sch
            .uniqueness_constraints
            .iter()
            .any(|u| u.name.as_deref() == Some(name))
        || sch.checks.iter().any(|c| c.name.as_deref() == Some(name))
}

/// v7.39 (read01 round 58) — lowercase hex, for the synthetic credential a
/// passwordless `CREATE ROLE` gets (it can't log in, but the record must not
/// carry an empty password).
fn hex_of(bytes: &[u8]) -> alloc::string::String {
    use core::fmt::Write as _;
    let mut s = alloc::string::String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// v7.39 (round 282) — render one argument type the way PG's NOTICE does.
///
/// PG's grammar has two productions for a type name: the SQL-standard
/// KEYWORDS (`int`, `character varying`, `double precision`, …) become a
/// `SystemTypeName`, which deparses schema-qualified with the internal
/// name — `pg_catalog.int4`; anything else is an ordinary identifier and
/// survives verbatim. So `int` prints as `pg_catalog.int4` while the
/// equally valid `int4` prints as `int4`, and `date` — not a type keyword
/// in that production — prints as `date`. Every entry below was read off
/// live PG 18.4 rather than inferred from the list's shape.
fn pg_signature_type_name(raw: &str) -> alloc::string::String {
    let mut norm = alloc::string::String::new();
    for word in raw.split_whitespace() {
        if !norm.is_empty() {
            norm.push(' ');
        }
        norm.push_str(&word.to_ascii_lowercase());
    }
    let internal = match norm.as_str() {
        "int" | "integer" => "int4",
        "smallint" => "int2",
        "bigint" => "int8",
        "real" => "float4",
        "float" | "double precision" => "float8",
        "decimal" | "dec" | "numeric" => "numeric",
        "boolean" => "bool",
        "varchar" | "character varying" => "varchar",
        "char" | "character" => "bpchar",
        "time" | "time without time zone" => "time",
        "time with time zone" => "timetz",
        "timestamp" | "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "interval" => "interval",
        "bit" => "bit",
        "bit varying" => "varbit",
        _ => return raw.into(),
    };
    alloc::format!("pg_catalog.{internal}")
}

/// v7.39 (round 735, S14/B3) — the FULL set of stored tables a
/// materialized-view body reads, or `None` when that set cannot be
/// PROVEN (CTEs, unions, subqueries anywhere, any non-table FROM
/// source, a join whose ON carries a subquery…). `None` means "always
/// refresh fully" — the conservative direction; an under-collected set
/// here would be a WRONG no-op serving stale data, so every uncertain
/// shape bails.
impl Engine {
    /// v7.39 (round 737, S14/B3 knife 2) — run buffered INSERTs through
    /// the view's projection and append the survivors. The body is a
    /// registered-maintainable single-table pure projection, so each new
    /// base row maps to at most one view row: eval the WHERE (absent =
    /// keep), then each item, against the base row.
    /// v7.39 (round 738) — apply buffered changes in ARRIVAL order.
    /// `Ok(None)` = this buffer cannot be applied incrementally (an
    /// Update change; or a delete/tombstone with no valid row map) —
    /// the caller takes the full path. Inserts run the projection and
    /// append; deletes and tombstones resolve base RowIds through the
    /// row map and remove the view rows, keeping the map's positions
    /// and expected length exact after every step.
    fn apply_matview_delta_ordered(
        &mut self,
        name: &str,
        body: &spg_sql::ast::SelectStatement,
        buf: &[spg_storage::RowChange],
    ) -> Result<Option<usize>, EngineError> {
        use spg_sql::ast::SelectItem;
        let needs_map = buf
            .iter()
            .any(|c| !matches!(c, spg_storage::RowChange::Insert { .. }));
        if needs_map {
            let Some((expected, _)) = self.matview_row_map.get(name) else {
                return Ok(None);
            };
            let live = self
                .active_catalog()
                .get(name)
                .map(|t| t.rows().len())
                .unwrap_or(usize::MAX);
            if live != *expected {
                // A vacuum (or anything else) moved the backing rows.
                self.matview_row_map.remove(name);
                return Ok(None);
            }
        }
        let base = self
            .matview_maintainable
            .get(name)
            .cloned()
            .expect("caller checked registration");
        let base_cols = self
            .active_catalog()
            .get(&base)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "materialized view {name:?} base table {base:?} missing"
                ))
            })?
            .schema()
            .columns
            .clone();
        let alias = body
            .from
            .as_ref()
            .and_then(|f| f.primary.alias.clone())
            .unwrap_or_else(|| base.clone());
        let mut applied = 0usize;
        for ch in buf {
            match ch {
                spg_storage::RowChange::Insert { row, rowid, .. } => {
                    let keep = if let Some(w) = &body.where_ {
                        let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
                        let cond = eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                        crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)?
                    } else {
                        true
                    };
                    if !keep {
                        continue;
                    }
                    let mut vals = alloc::vec::Vec::with_capacity(body.items.len());
                    {
                        let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
                        for item in &body.items {
                            let SelectItem::Expr { expr, .. } = item else {
                                unreachable!("registration admits Expr items only");
                            };
                            vals.push(eval::eval_expr(expr, row, &ctx).map_err(EngineError::Eval)?);
                        }
                    }
                    let cat = self.active_catalog_mut();
                    let table = cat.get_mut(name).ok_or_else(|| {
                        EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                            "materialized view {name:?} backing table missing"
                        )))
                    })?;
                    table
                        .insert(spg_storage::Row::new(vals))
                        .map_err(EngineError::Storage)?;
                    let new_pos = table.rows().len() - 1;
                    if let Some((expected, map)) = self.matview_row_map.get_mut(name) {
                        map.insert(rowid.0, new_pos);
                        *expected += 1;
                    }
                    applied += 1;
                }
                spg_storage::RowChange::Delete { rowids, .. }
                | spg_storage::RowChange::Tombstone { rowids, .. } => {
                    // v7.39 (round 740) — TOMBSTONE the view row, never
                    // physically remove it. delete_rows on a mid-table
                    // position is O(table) in the persistent vec, and
                    // every surviving map entry would need shifting —
                    // measured 70 ms for THREE deletes over a 250k-row
                    // view. A tombstone is O(1), keeps every physical
                    // position (the map needs no shift and `expected`
                    // means what it says), and the view's readers
                    // already gate on MVCC visibility like any table.
                    // Vacuumed/compacted views change their length and
                    // the expected-length check catches it -> full.
                    for rid in rowids {
                        let Some((_, map)) = self.matview_row_map.get_mut(name) else {
                            unreachable!("needs_map gated above");
                        };
                        let Some(pos) = map.remove(&rid.0) else {
                            // A base row the WHERE filtered out — the
                            // view never held it; nothing to remove.
                            continue;
                        };
                        let v = self.writer_version_for_current_stmt();
                        let cat = self.active_catalog_mut();
                        let table = cat.get_mut(name).ok_or_else(|| {
                            EngineError::Storage(spg_storage::StorageError::Corrupt(
                                alloc::format!("materialized view {name:?} backing table missing"),
                            ))
                        })?;
                        let _ = table.mark_row_deleted(pos, v);
                        applied += 1;
                    }
                }
                // v7.39 (round 739) — the Update arm: four quadrants of
                // (was the OLD row in the view?) x (does the NEW row
                // pass the WHERE?). In-place replacement keeps the map
                // untouched; a row leaving the view removes + shifts; a
                // row entering appends + records.
                spg_storage::RowChange::Update { new_row, rowid, .. } => {
                    let keep = if let Some(w) = &body.where_ {
                        let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
                        let r = spg_storage::Row::new(new_row.clone());
                        let cond = eval::eval_expr(w, &r, &ctx).map_err(EngineError::Eval)?;
                        crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)?
                    } else {
                        true
                    };
                    let old_pos = self
                        .matview_row_map
                        .get(name)
                        .and_then(|(_, m)| m.get(&rowid.0).copied());
                    match (old_pos, keep) {
                        (Some(pos), true) => {
                            let mut vals = alloc::vec::Vec::with_capacity(body.items.len());
                            {
                                let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
                                let r = spg_storage::Row::new(new_row.clone());
                                for item in &body.items {
                                    let SelectItem::Expr { expr, .. } = item else {
                                        unreachable!("registration admits Expr items only");
                                    };
                                    vals.push(
                                        eval::eval_expr(expr, &r, &ctx)
                                            .map_err(EngineError::Eval)?,
                                    );
                                }
                            }
                            let cat = self.active_catalog_mut();
                            let table = cat.get_mut(name).ok_or_else(|| {
                                EngineError::Storage(spg_storage::StorageError::Corrupt(
                                    alloc::format!(
                                        "materialized view {name:?} backing table missing"
                                    ),
                                ))
                            })?;
                            table.update_row(pos, vals).map_err(EngineError::Storage)?;
                            applied += 1;
                        }
                        (Some(pos), false) => {
                            let (_, map) = self
                                .matview_row_map
                                .get_mut(name)
                                .expect("needs_map gated above");
                            map.remove(&rowid.0);
                            let v = self.writer_version_for_current_stmt();
                            let cat = self.active_catalog_mut();
                            let table = cat.get_mut(name).ok_or_else(|| {
                                EngineError::Storage(spg_storage::StorageError::Corrupt(
                                    alloc::format!(
                                        "materialized view {name:?} backing table missing"
                                    ),
                                ))
                            })?;
                            let _ = table.mark_row_deleted(pos, v);
                            applied += 1;
                        }
                        (None, true) => {
                            let mut vals = alloc::vec::Vec::with_capacity(body.items.len());
                            {
                                let ctx = self.ev_ctx(&base_cols, Some(alias.as_str()));
                                let r = spg_storage::Row::new(new_row.clone());
                                for item in &body.items {
                                    let SelectItem::Expr { expr, .. } = item else {
                                        unreachable!("registration admits Expr items only");
                                    };
                                    vals.push(
                                        eval::eval_expr(expr, &r, &ctx)
                                            .map_err(EngineError::Eval)?,
                                    );
                                }
                            }
                            let cat = self.active_catalog_mut();
                            let table = cat.get_mut(name).ok_or_else(|| {
                                EngineError::Storage(spg_storage::StorageError::Corrupt(
                                    alloc::format!(
                                        "materialized view {name:?} backing table missing"
                                    ),
                                ))
                            })?;
                            table
                                .insert(spg_storage::Row::new(vals))
                                .map_err(EngineError::Storage)?;
                            let new_pos = table.rows().len() - 1;
                            let (expected, map) = self
                                .matview_row_map
                                .get_mut(name)
                                .expect("needs_map gated above");
                            map.insert(rowid.0, new_pos);
                            *expected += 1;
                            applied += 1;
                        }
                        (None, false) => {}
                    }
                }
            }
        }
        Ok(Some(applied))
    }
}

/// v7.39 (round 737, S14/B3 knife 2) — the base table of a
/// DELTA-MAINTAINABLE view body, or None. Strictly narrower than
/// `matview_dep_tables`: ONE stored table, pure projection items, a
/// pure WHERE, and none of the shapes whose delta is not row-local
/// (aggregates / GROUP BY / DISTINCT [ON] / ORDER / LIMIT / OFFSET /
/// windows / SRFs — plus everything the dep collector already bails
/// on). Anything outside refreshes fully, as today.
fn matview_maintainable_base(stmt: &spg_sql::ast::SelectStatement) -> Option<String> {
    use spg_sql::ast::SelectItem;
    let deps = matview_dep_tables(stmt)?;
    if deps.len() != 1 {
        return None;
    }
    if stmt.distinct
        || !stmt.distinct_on.is_empty()
        || stmt.group_by.is_some()
        || stmt.group_by_all
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.window_check_exprs.is_empty()
        || crate::aggregate::uses_aggregate(stmt)
        || crate::window::select_has_window(stmt)
    {
        return None;
    }
    for item in &stmt.items {
        let SelectItem::Expr { expr, .. } = item else {
            return None;
        };
        if !crate::eval::fully_compilable(expr) || crate::select::expr_contains_builtin_srf(expr) {
            return None;
        }
    }
    if let Some(w) = &stmt.where_
        && !crate::eval::fully_compilable(w)
    {
        return None;
    }
    deps.into_iter().next()
}

fn matview_dep_tables(
    stmt: &spg_sql::ast::SelectStatement,
) -> Option<alloc::collections::BTreeSet<String>> {
    use spg_sql::ast::SelectItem;
    if !stmt.ctes.is_empty() || !stmt.unions.is_empty() {
        return None;
    }
    let from = stmt.from.as_ref()?;
    let mut out = alloc::collections::BTreeSet::new();
    let mut take = |t: &spg_sql::ast::TableRef| -> bool {
        if t.name.is_empty()
            || t.lateral_subquery.is_some()
            || t.unnest_expr.is_some()
            || t.generate_series_args.is_some()
            || t.as_of_segment.is_some()
            || t.jsonb_each_text_arg.is_some()
            || t.table_fn_call.is_some()
            || t.rows_from.is_some()
            || t.json_table.is_some()
        {
            return false;
        }
        out.insert(t.name.to_ascii_lowercase());
        true
    };
    if !take(&from.primary) {
        return None;
    }
    for j in &from.joins {
        if !take(&j.table) {
            return None;
        }
        if j.on.as_ref().is_some_and(crate::expr_has_subquery) {
            return None;
        }
    }
    let any_sub = stmt.items.iter().any(|i| match i {
        SelectItem::Expr { expr, .. } => crate::expr_has_subquery(expr),
        _ => false,
    }) || stmt.where_.as_ref().is_some_and(crate::expr_has_subquery)
        || stmt
            .group_by
            .as_ref()
            .is_some_and(|gs| gs.iter().any(crate::expr_has_subquery))
        || stmt.having.as_ref().is_some_and(crate::expr_has_subquery)
        || stmt
            .order_by
            .iter()
            .any(|o| crate::expr_has_subquery(&o.expr));
    if any_sub {
        return None;
    }
    Some(out)
}
