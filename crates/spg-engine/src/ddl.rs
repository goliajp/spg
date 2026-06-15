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
    Statement, VecEncoding as SqlVecEncoding,
};
use spg_storage::{ColumnSchema, DataType, StorageError, TableSchema, Value, VecEncoding};

use crate::{
    CancelToken, ClockFn, Engine, EngineError, QueryResult, check_existing_unique_violation,
    coerce_value, column_type_to_data_type, enforce_fk_inserts, eval, infer_column_types,
    literal_expr_to_value, resolve_foreign_key, rewrite_column_in_source, users,
};

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
        for target in s.targets {
            self.exec_alter_table_subaction(&table_name, target)?;
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
            T::SetHotTierBytes(n) => self.alter_set_hot_tier_bytes(tbl, n),
            T::AddForeignKey(fk) => self.alter_add_foreign_key(tbl, fk),
            T::DropForeignKey { name, if_exists } => {
                self.alter_drop_foreign_key(tbl, name, if_exists)
            }
            T::AddColumn {
                column,
                if_not_exists,
            } => self.alter_add_column(tbl, column, if_not_exists),
            T::AlterColumnType {
                column,
                new_type,
                using,
            } => self.alter_column_type(tbl, column, new_type, using),
            T::AddTableConstraint(tc) => self.alter_add_table_constraint(tbl, tc),
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
        }
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
        let existing_rows: Vec<Vec<Value>> = self
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
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE ADD CONSTRAINT: a constraint named {name:?} already exists"
            )));
        }
        table.schema_mut().foreign_keys.push(storage_fk);
        Ok(())
    }

    fn alter_drop_foreign_key(
        &mut self,
        tbl: &str,
        name: String,
        if_exists: bool,
    ) -> Result<(), EngineError> {
        let table = self.active_catalog_mut().get_mut(tbl).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: tbl.into() })
        })?;
        let fks = &mut table.schema_mut().foreign_keys;
        let before = fks.len();
        fks.retain(|f| f.name.as_ref() != Some(&name));
        if fks.len() == before && !if_exists {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE DROP CONSTRAINT: no FK named {name:?} on {:?}",
                tbl
            )));
        }
        // v7.13.2 mailrs round-6 S7: IF EXISTS silences the miss.
        Ok(())
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
                return Ok(());
            }
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE ADD COLUMN: column {:?} already exists on {:?}",
                column.name,
                tbl
            )));
        }
        let col_name = column.name.clone();
        let nullable = column.nullable;
        let has_default = column.default.is_some() || column.auto_increment;
        let col_schema = column_def_to_schema(column)?;
        let row_count = table.row_count();
        // Compute the back-fill value. Literal / runtime DEFAULT
        // funnels through the same resolver that INSERT uses
        // (v7.9.21 `resolve_column_default_free`). NULL when
        // the column is nullable and has no DEFAULT. NOT NULL
        // without DEFAULT errors when the table has existing
        // rows — same as PG.
        let fill_value: Value = if has_default || col_schema.runtime_default.is_some() {
            resolve_column_default_free(&col_schema, clock)?
        } else if nullable || row_count == 0 {
            Value::Null
        } else {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE ADD COLUMN {col_name:?}: NOT NULL column requires DEFAULT \
                         when the table has existing rows"
            )));
        };
        table.add_column(col_schema, fill_value);
        Ok(())
    }

    fn alter_column_type(
        &mut self,
        tbl: &str,
        column: String,
        new_type: spg_sql::ast::ColumnTypeName,
        using: Option<Expr>,
    ) -> Result<(), EngineError> {
        // v7.13.0 — mailrs round-5 G8. Re-evaluate each
        // row's column value (either through the USING
        // expression if supplied, or as a direct CAST of
        // the existing value) and re-coerce to the new
        // type. Indices on the column get rebuilt.
        let new_data_type = column_type_to_data_type(new_type);
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
                    "ALTER COLUMN TYPE: column {column:?} not found on {:?}",
                    tbl
                ))
            })?;
        let schema_cols = table.schema().columns.clone();
        let ctx = eval::EvalContext::new(&schema_cols, None);
        let mut new_values: alloc::vec::Vec<Value> =
            alloc::vec::Vec::with_capacity(table.row_count());
        for row in table.rows().iter() {
            let raw = match &using {
                Some(expr) => eval::eval_expr(expr, row, &ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "ALTER COLUMN TYPE: USING expression failed: {e:?}"
                    ))
                })?,
                None => row.values.get(col_pos).cloned().unwrap_or(Value::Null),
            };
            let coerced = coerce_value(raw, new_data_type, &column, col_pos)?;
            new_values.push(coerced);
        }
        table.schema_mut().columns[col_pos].ty = new_data_type;
        for (i, v) in new_values.into_iter().enumerate() {
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
            spg_sql::ast::TableConstraint::Check { expr, .. } => {
                table.schema_mut().checks.push(alloc::format!("{expr}"));
            }
            spg_sql::ast::TableConstraint::Index { name, columns } => {
                // v7.15.0 — ALTER TABLE ADD KEY (cols).
                // mysqldump occasionally emits this
                // post-CREATE-TABLE shape; build a BTree
                // on the leading column using the
                // user-supplied or synthesised name.
                let leading = &columns[0];
                let already_idx = table.indices().iter().any(|idx| {
                    matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                        && table.schema().columns[idx.column_position].name == *leading
                });
                if !already_idx {
                    let idx_name = name
                        .clone()
                        .unwrap_or_else(|| alloc::format!("{}_{leading}_idx", tbl));
                    let _ = table.add_index(idx_name, leading);
                }
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
                    return Ok(());
                }
                return Err(EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE DROP COLUMN: column {column:?} not found on {:?}",
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

    fn alter_rename_table(&mut self, tbl: &str, new: String) -> Result<(), EngineError> {
        // v7.16.2 — table-level rename (mailrs round-10
        // A.5 — used by migrate-042's `ALTER TABLE
        // contacts RENAME TO email_contacts`). Storage
        // helper updates the schema + by_name index +
        // dangling FK / trigger references in one
        // atomic step.
        let old = tbl.to_string();
        self.active_catalog_mut()
            .rename_table(&old, &new)
            .map_err(EngineError::Storage)?;
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
                EngineError::Unsupported(alloc::format!(
                    "ALTER TABLE RENAME COLUMN: column {old:?} not found on {:?}",
                    tbl
                ))
            })?;
        // Reject same-name (case-insensitive) collision.
        if table
            .schema()
            .columns
            .iter()
            .enumerate()
            .any(|(i, c)| i != col_pos && c.name.eq_ignore_ascii_case(&new))
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "ALTER TABLE RENAME COLUMN: column {new:?} already exists on {:?}",
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
            new_checks.push(rewrite_column_in_source(&chk, &old, &new)?);
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
                    modified_catalog: !self.in_transaction(),
                }),
                Err(StorageError::IndexNotFound { .. }) if if_exists => {
                    Ok(QueryResult::CommandOk {
                        affected: 0,
                        modified_catalog: false,
                    })
                }
                Err(e) => Err(EngineError::Storage(e)),
            };
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
            modified_catalog: !self.in_transaction(),
        })
    }

    pub(crate) fn exec_create_index(
        &mut self,
        stmt: CreateIndexStatement,
    ) -> Result<QueryResult, EngineError> {
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
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        // v7.9.14 — multi-column index parses through; engine
        // builds a single-column BTree on the leading column only.
        // The extras live on the AST so spg-server's dispatcher
        // can emit a PG-wire NoticeResponse / log line. Composite
        // BTree keys land in v7.10.
        let _ = &stmt.extra_columns; // intentional drop on engine side
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
        match stmt.method {
            IndexMethod::BTree => table.add_index(stmt.name.clone(), &stmt.column)?,
            IndexMethod::Hnsw => {
                if !included_positions.is_empty() {
                    return Err(EngineError::Unsupported(
                        "INCLUDE columns are not supported on HNSW indexes".into(),
                    ));
                }
                table.add_nsw_index(stmt.name.clone(), &stmt.column, spg_storage::NSW_DEFAULT_M)?;
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
                let col_pos = table
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
                } else {
                    // v7.9.26b BTree fallback — the catalog still
                    // gets an index entry on the leading column so
                    // pg_dump scripts that name GIN on JSONB / etc.
                    // load clean; query-time gain stays opt-in for
                    // tsvector callers.
                    table.add_index(stmt.name.clone(), &stmt.column)?;
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
            if matches!(
                stmt.method,
                IndexMethod::Hnsw | IndexMethod::Brin | IndexMethod::Gin
            ) {
                return Err(EngineError::Unsupported(
                    "Expression keys are not supported on HNSW or BRIN indexes".into(),
                ));
            }
            let canonical = key_expr.to_string();
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.expression = Some(canonical);
            }
        }
        // v7.9.29 — persist `is_unique` flag on the storage Index.
        // Combined with `partial_predicate`, INSERT enforcement
        // checks that no other row whose predicate evaluates true
        // shares the same indexed key. Parser already rejected
        // `UNIQUE` on HNSW / BRIN, so plain BTree here.
        // For multi-column UNIQUE INDEX the extras matter (the
        // full tuple is the uniqueness key), so resolve them to
        // column positions and persist on the index too.
        if stmt.is_unique {
            let mut extra_positions: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
            for col_name in &stmt.extra_columns {
                let pos = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "UNIQUE INDEX {:?}: extra column {col_name:?} not in table {:?}",
                            stmt.name,
                            stmt.table
                        ))
                    })?;
                extra_positions.push(pos);
            }
            if let Some(idx) = table.indices_mut().iter_mut().find(|i| i.name == stmt.name) {
                idx.is_unique = true;
                idx.extra_column_positions = extra_positions;
            }
            // At index-creation time, check the existing rows for
            // pre-existing duplicates that would have violated the
            // new constraint — otherwise CREATE UNIQUE INDEX would
            // silently leave duplicates in place.
            let snapshot_indices = table.indices().to_vec();
            let snapshot_rows: alloc::vec::Vec<spg_storage::Row> =
                table.rows().iter().cloned().collect();
            let snapshot_schema = table.schema().clone();
            let idx_ref = snapshot_indices
                .iter()
                .find(|i| i.name == stmt.name)
                .expect("just-added index");
            check_existing_unique_violation(idx_ref, &snapshot_schema, &snapshot_rows)?;
        }
        // v6.3.1 — adding an index can change the optimal plan for
        // any cached query that references this table.
        self.plan_cache.evict_referencing(&table_name);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
            let col_schema = column_def_to_schema(col_def)?;
            let fill_value: Value = if has_default || col_schema.runtime_default.is_some() {
                resolve_column_default_free(&col_schema, clock)?
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
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.14.0 — DROP TABLE handler (pg_dump / mysqldump preamble).
    pub(crate) fn exec_drop_table(
        &mut self,
        names: Vec<String>,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        for name in names {
            let dropped = self.active_catalog_mut().drop_table(&name);
            if !dropped && !if_exists {
                return Err(EngineError::Storage(StorageError::TableNotFound { name }));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.14.0 — DROP INDEX handler.
    pub(crate) fn exec_drop_index(
        &mut self,
        name: String,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let dropped = self.active_catalog_mut().drop_named_index(&name);
        if !dropped && !if_exists {
            return Err(EngineError::Storage(StorageError::IndexNotFound { name }));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    pub(crate) fn exec_create_table(
        &mut self,
        stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        if stmt.if_not_exists && self.active_catalog().get(&stmt.name).is_some() {
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
        let schema = self.build_create_table_schema(
            &table_name,
            stmt.columns,
            &stmt.table_constraints,
            stmt.foreign_keys,
            &inline_pk_columns,
        )?;
        self.active_catalog_mut().create_table(schema)?;
        self.install_implicit_indexes(&table_name, &inline_pk_columns, &stmt.table_constraints)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// Build the `TableSchema` for a CREATE TABLE: column schemas with
    /// ENUM / DOMAIN bindings resolved, table-level + inline PRIMARY KEY
    /// NOT NULL marking, FK resolution (deferring to `pending_foreign_keys`
    /// when checks are off and the parent is absent), and uniqueness /
    /// CHECK constraint translation.
    #[allow(clippy::too_many_lines)]
    fn build_create_table_schema(
        &mut self,
        table_name: &str,
        columns: Vec<ColumnDef>,
        table_constraints: &[spg_sql::ast::TableConstraint],
        foreign_keys: Vec<spg_sql::ast::ForeignKeyConstraint>,
        inline_pk_columns: &[String],
    ) -> Result<TableSchema, EngineError> {
        // v7.9.19 — table-level constraints: PRIMARY KEY (a, b, ...)
        // and UNIQUE (a, b, ...). Each builds a BTree index on the
        // leading column (the existing single-column storage tier)
        // and registers a UniquenessConstraint on the schema for
        // INSERT-time enforcement of the full tuple. mailrs G1/G6.
        let cols = columns
            .into_iter()
            .map(column_def_to_schema)
            .collect::<Result<Vec<_>, _>>()?;
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
                col.ty = dom.base_type;
                col.user_domain_type = Some(name);
                if !dom.nullable {
                    col.nullable = false;
                }
                continue;
            }
            return Err(EngineError::Unsupported(alloc::format!(
                "column {:?}: unknown column type {:?} (not a built-in, ENUM, or DOMAIN)",
                col.name,
                name
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
        schema.foreign_keys = fks;
        // v7.9.19 — translate AST table_constraints to storage
        // UniquenessConstraints (column name → position) so the
        // INSERT enforcement helper sees positions directly.
        let mut uc_storage: Vec<spg_storage::UniquenessConstraint> = Vec::new();
        let mut check_exprs: Vec<String> = Vec::new();
        for tc in table_constraints {
            let (is_pk, names, nnd) = match tc {
                spg_sql::ast::TableConstraint::PrimaryKey { columns, .. } => {
                    (true, columns.clone(), false)
                }
                spg_sql::ast::TableConstraint::Unique {
                    columns,
                    nulls_not_distinct,
                    ..
                } => (false, columns.clone(), *nulls_not_distinct),
                spg_sql::ast::TableConstraint::Check { expr, .. } => {
                    // v7.13.0 — collect CHECK predicate sources;
                    // they get attached to the schema below.
                    check_exprs.push(alloc::format!("{expr}"));
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
                });
            }
        }
        schema.uniqueness_constraints = uc_storage.clone();
        schema.checks = check_exprs;
        Ok(schema)
    }

    /// Install the implicit BTree / fulltext-GIN indexes a freshly-created
    /// table needs: one per inline PRIMARY KEY column, plus one per
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
        for (i, col_name) in inline_pk_columns.iter().enumerate() {
            let idx_name = if inline_pk_columns.len() == 1 {
                alloc::format!("{table_name}_pkey")
            } else {
                alloc::format!("{table_name}_pkey_{i}")
            };
            if let Err(e) = table.add_index(idx_name, col_name) {
                return Err(EngineError::Storage(e));
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
            };
            let leading = &names[0];
            // Skip if a same-column BTree already exists (e.g.
            // inline PK on the leading column).
            let already = table.indices().iter().any(|idx| {
                matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                    && table.schema().columns[idx.column_position].name == *leading
            });
            if already {
                continue;
            }
            let idx_name = if let Some(n) = explicit_name {
                n.clone()
            } else if names.len() == 1 {
                alloc::format!("{table_name}_{leading}_{suffix}")
            } else {
                alloc::format!("{table_name}_{leading}_{suffix}_{i}")
            };
            if let Err(e) = table.add_index(idx_name, leading) {
                return Err(EngineError::Storage(e));
            }
        }
        Ok(())
    }
}

impl Engine {
    pub(crate) fn exec_create_user(
        &mut self,
        s: &CreateUserStatement,
    ) -> Result<QueryResult, EngineError> {
        if self.in_transaction() {
            return Err(EngineError::Unsupported(
                "CREATE USER is not allowed inside a transaction".into(),
            ));
        }
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
        self.users
            .create(&s.name, &s.password, role, salt)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE USER: {e}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_drop_user(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        if self.in_transaction() {
            return Err(EngineError::Unsupported(
                "DROP USER is not allowed inside a transaction".into(),
            ));
        }
        self.users
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
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("trigger {name:?} on {table:?} does not exist"),
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
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let removed = self.active_catalog_mut().drop_function(name);
        if !removed && !if_exists {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("function {name:?} does not exist"),
            )));
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
        if start < min_value || start > max_value {
            return Err(EngineError::Unsupported(alloc::format!(
                "START WITH ({start}) is outside MINVALUE..MAXVALUE ({min_value}..{max_value})"
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
        };
        self.active_catalog_mut()
            .create_sequence(def, s.if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
        let cat = self.active_catalog_mut();
        if !cat.sequences().contains_key(&s.name) {
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
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.17.0 Phase 1.2 — `CREATE VIEW` engine path. Stores the
    /// Display-rendered body verbatim in the catalog; SELECT-from-
    /// view at exec time re-parses + prepends as a synthetic CTE.
    pub(crate) fn exec_create_view(
        &mut self,
        s: spg_sql::ast::CreateViewStatement,
    ) -> Result<QueryResult, EngineError> {
        // Render the SELECT body to canonical form so the catalog
        // round-trips a deterministic source (no whitespace /
        // comment surprises in the on-disk snapshot).
        let body_repr = alloc::format!("{}", spg_sql::ast::Statement::Select(s.body));
        let def = spg_storage::ViewDef {
            name: s.name.clone(),
            columns: s.columns,
            body: body_repr,
        };
        self.active_catalog_mut()
            .create_view(def, s.or_replace, s.if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
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
        if cat.sequences().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} would shadow an existing sequence", s.name),
            )));
        }
        if cat.views().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("type {:?} would shadow an existing view", s.name),
            )));
        }
        let def = match s.kind {
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
                spg_storage::EnumDef {
                    name: s.name.clone(),
                    labels,
                }
            }
        };
        self.active_catalog_mut()
            .create_enum_type(def)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
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
            || cat.sequences().contains_key(&s.name)
            || cat.views().contains_key(&s.name)
            || cat.enum_types().contains_key(&s.name)
        {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!("domain {:?} would shadow an existing object", s.name),
            )));
        }
        let base_type = column_type_to_data_type(s.base_type);
        let default = s.default.as_ref().map(|e| alloc::format!("{e}"));
        let checks = s
            .checks
            .iter()
            .map(|e| alloc::format!("{e}"))
            .collect::<Vec<_>>();
        let def = spg_storage::DomainDef {
            name: s.name.clone(),
            base_type,
            nullable: !s.not_null,
            default,
            checks,
        };
        self.active_catalog_mut()
            .create_domain_type(def)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
            modified_catalog: removed > 0 && !self.in_transaction(),
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
        self.active_catalog_mut()
            .create_schema(name, if_not_exists)
            .map_err(EngineError::Storage)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && !self.in_transaction(),
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
            let was_present = self.active_catalog_mut().drop_enum_type(name);
            if was_present {
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("type {name:?} does not exist"),
                )));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && !self.in_transaction(),
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
        if cat.views().contains_key(&s.name) {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                alloc::format!(
                    "materialized view {:?} would shadow an existing view",
                    s.name
                ),
            )));
        }
        if cat.sequences().contains_key(&s.name) {
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
        if s.with_data {
            let table = cat
                .get_mut(&s.name)
                .expect("just-created materialized-view backing table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
            }
        }
        cat.register_materialized_view(s.name.clone(), body_repr);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
        let source = self
            .active_catalog()
            .materialized_views()
            .get(name)
            .cloned()
            .ok_or_else(|| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                    "materialized view {name:?} does not exist"
                )))
            })?;
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
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: !self.in_transaction(),
            });
        }
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
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
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
                removed += 1;
            } else if !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("materialized view {name:?} does not exist"),
                )));
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && !self.in_transaction(),
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
            let was_present = self.active_catalog_mut().drop_view(name);
            if !was_present && !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("view {name:?} does not exist"),
                )));
            }
            if was_present {
                removed += 1;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && !self.in_transaction(),
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
            let was_present = self.active_catalog_mut().drop_sequence(name);
            if !was_present && !if_exists {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    alloc::format!("sequence {name:?} does not exist"),
                )));
            }
            if was_present {
                removed += 1;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: removed,
            modified_catalog: removed > 0 && !self.in_transaction(),
        })
    }
}

// ---- column-definition / DEFAULT / SET / enum helpers (lib.rs split 11) ----

/// v7.9.21 — resolve a column's DEFAULT for INSERT-time
/// default-fill. Free fn (rather than `&self`) so callers
/// with an active `&mut Table` borrow can still use it.
/// Literal defaults take the cached path (`col.default`);
/// runtime defaults hit `clock_fn` at each call. mailrs G4.
pub(crate) fn resolve_column_default_free(
    col: &ColumnSchema,
    clock_fn: Option<ClockFn>,
) -> Result<Value, EngineError> {
    if let Some(rt) = &col.runtime_default {
        return eval_runtime_default_free(rt, col.ty, clock_fn);
    }
    Ok(col.default.clone().unwrap_or(Value::Null))
}

pub(crate) fn eval_runtime_default_free(
    rt: &str,
    ty: DataType,
    clock_fn: Option<ClockFn>,
) -> Result<Value, EngineError> {
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
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "runtime DEFAULT expression {other:?} not supported \
                 (v7.17.0 whitelist: now() / current_timestamp / \
                 current_date / current_time / localtimestamp / \
                 localtime / gen_random_uuid() / \
                 uuid_generate_v4())"
            )));
        }
    };
    coerce_value(v, ty, "DEFAULT", 0)
}

/// v7.9.21 — true when a DEFAULT expression needs INSERT-time
/// evaluation rather than being cacheable as a literal Value.
/// FunctionCall is the immediate case (`now()`,
/// `current_timestamp`). Literal expressions and simple sign-
/// flipped numerics still take the static-cache path.
fn is_runtime_default_expr(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { .. } => true,
        Expr::Unary { expr, .. } => is_runtime_default_expr(expr),
        _ => false,
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
    value: Value,
) -> Result<Value, EngineError> {
    let Some(variants) = lookup.get(&col_idx) else {
        return Ok(value);
    };
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => {
            if s.is_empty() {
                return Ok(Value::Text(alloc::string::String::new()));
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
            Ok(Value::Text(out))
        }
        other => Err(EngineError::Unsupported(alloc::format!(
            "column {col_name:?}: SET-typed column expects TEXT, got {:?}",
            other.data_type()
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
                "column {col_name:?}: enum-typed column expects TEXT, got {:?}",
                other.data_type()
            ))),
        }
    } else {
        Ok(())
    }
}

fn column_def_to_schema(c: ColumnDef) -> Result<ColumnSchema, EngineError> {
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
    schema.collation = match c.collation {
        spg_sql::ast::Collation::Binary => spg_storage::Collation::Binary,
        spg_sql::ast::Collation::CaseInsensitive => spg_storage::Collation::CaseInsensitive,
    };
    // v7.17.0 Phase 4.4 — MySQL `UNSIGNED` flag propagates to
    // storage so engine INSERT / UPDATE can range-check.
    schema.is_unsigned = c.is_unsigned;
    // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM variant list.
    // INSERT validation lives in coerce_value (Text → Text path
    // with the column's variant list as the accept-set).
    schema.inline_enum_variants = c.inline_enum_variants;
    // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant list.
    // INSERT canonicalisation (de-dup + sort by definition order)
    // lives in the exec_insert path next to the ENUM check.
    schema.inline_set_variants = c.inline_set_variants;
    if let Some(default_expr) = c.default {
        // v7.9.21 — distinguish literal defaults (evaluated once
        // at CREATE TABLE) from expression defaults (deferred to
        // INSERT). Function calls (`now()`, `current_timestamp`
        // — see v7.9.20 keyword promotion) take the runtime path.
        // Literals continue to cache. mailrs G4.
        if is_runtime_default_expr(&default_expr) {
            let display = alloc::format!("{default_expr}");
            schema = schema.with_runtime_default(display);
        } else {
            let raw = literal_expr_to_value(default_expr)?;
            let coerced = coerce_value(raw, ty, &c.name, 0)?;
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
