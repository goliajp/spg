//! Read-only / snapshot execution, split out of `lib.rs` (lib.rs split
//! 18). Two entry families share one module: the live read path
//! (`execute_readonly` / `_with_cancel`, taken by the server under an
//! `RwLock::read()` so SELECTs run in parallel) and the snapshot path
//! (`execute_readonly_on_snapshot` / the prepared + describe variants /
//! `is_readonly_sql` / `prepare_on_snapshot`), which run against a
//! `CatalogSnapshot` without borrowing the engine. Both reject DDL/DML
//! with `WriteRequired` and route SELECT / SHOW / EXPLAIN to the same
//! domain handlers as the write path. Whole `impl Engine` methods; the
//! public surface is unchanged, and `enforce_row_limit` stays in the
//! crate root (shared with `execute.rs`, reached via self).

use alloc::vec::Vec;

use spg_sql::ast::Statement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{ColumnSchema, Value};

use crate::describe;
use crate::{
    CancelToken, CatalogSnapshot, Engine, EngineError, QueryResult, expand_group_by_all, reorder,
    resolve_order_by_position, rewrite_clock_calls, substitute_placeholders,
};

impl Engine {
    /// v7.11.1 — execute a read-only SQL statement against a
    /// `CatalogSnapshot` without touching this engine. Same
    /// semantics as `execute_readonly` but parameterised on the
    /// snapshot's catalog. Reject DDL/DML the same way
    /// `execute_readonly` does. Static-on-Self so the caller can
    /// dispatch without holding an `Engine` borrow alongside the
    /// snapshot.
    pub fn execute_readonly_on_snapshot(
        snapshot: &CatalogSnapshot,
        sql: &str,
    ) -> Result<QueryResult, EngineError> {
        Self::execute_readonly_on_snapshot_with_cancel(snapshot, sql, CancelToken::none())
    }

    /// v7.11.1 — `execute_readonly_on_snapshot` with cooperative
    /// cancellation. Builds a transient `Engine` over the snapshot
    /// state, runs `execute_readonly_with_cancel`, drops. The
    /// transient engine is cheap to construct (no I/O; everything
    /// is just struct moves) and lets the existing read path stay
    /// untouched.
    pub fn execute_readonly_on_snapshot_with_cancel(
        snapshot: &CatalogSnapshot,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let transient = Engine {
            catalog: snapshot.catalog.clone(),
            statistics: snapshot.statistics.clone(),
            clock: snapshot.clock,
            max_query_rows: snapshot.max_query_rows,
            ..Engine::default()
        };
        transient.execute_readonly_with_cancel(sql, cancel)
    }

    /// v7.18 — execute a previously-prepared `Statement` against a
    /// `CatalogSnapshot` in read-only mode. Mirror of
    /// [`Engine::execute_prepared`] for the fan-out read path:
    /// substitutes `Expr::Placeholder(n)` nodes from `params`, then
    /// dispatches through [`Engine::execute_readonly_stmt_with_cancel`]
    /// (writes / DDL hit `EngineError::WriteRequired`). Static-on-Self
    /// so multiple readonly threads can dispatch against the same
    /// snapshot concurrently without an `Engine` borrow.
    ///
    /// **Schema drift contract**. The `Statement` was prepared against
    /// some prior catalog. If the snapshot's catalog has since
    /// diverged (DDL renamed / dropped a referenced column / table),
    /// execution surfaces the normal `EngineError` — same shape as
    /// PG's "cached plan must not change result type". Caller decides
    /// whether to re-prepare; engine does NOT auto-retry.
    pub fn execute_readonly_prepared_on_snapshot(
        snapshot: &CatalogSnapshot,
        stmt: Statement,
        params: &[Value],
    ) -> Result<QueryResult, EngineError> {
        Self::execute_readonly_prepared_on_snapshot_with_cancel(
            snapshot,
            stmt,
            params,
            CancelToken::none(),
        )
    }

    /// v7.18 — cancellable variant of
    /// [`Engine::execute_readonly_prepared_on_snapshot`].
    pub fn execute_readonly_prepared_on_snapshot_with_cancel(
        snapshot: &CatalogSnapshot,
        mut stmt: Statement,
        params: &[Value],
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        substitute_placeholders(&mut stmt, params)?;
        let transient = Engine {
            catalog: snapshot.catalog.clone(),
            statistics: snapshot.statistics.clone(),
            clock: snapshot.clock,
            max_query_rows: snapshot.max_query_rows,
            ..Engine::default()
        };
        transient.execute_readonly_stmt_with_cancel(stmt, cancel)
    }

    /// v7.18 — describe a prepared `Statement` against a
    /// `CatalogSnapshot`. Same `(parameter_oids, output_columns)`
    /// shape as [`Engine::describe_prepared`]; resolves names
    /// against the snapshot's catalog instead of `self`. Pure
    /// function — no engine state read.
    pub fn describe_prepared_on_snapshot(
        snapshot: &CatalogSnapshot,
        stmt: &Statement,
    ) -> (Vec<u32>, Vec<ColumnSchema>) {
        describe::describe_prepared(stmt, &snapshot.catalog)
    }

    /// v7.18 — does this SQL string classify as read-only? Parses
    /// `sql` with the engine parser and consults
    /// `Statement::is_readonly()`. A parse error returns `false`
    /// (route to the writer path so the user sees the canonical
    /// parse error from the writer's simple-query dispatch).
    /// Static-on-Self so the spg-sqlx connection layer can ask
    /// without an `Engine` borrow.
    #[must_use]
    pub fn is_readonly_sql(sql: &str) -> bool {
        parser::parse_statement(sql)
            .as_ref()
            .map(spg_sql::ast::Statement::is_readonly)
            .unwrap_or(false)
    }

    /// v7.18 — parse + plan a SQL string against a
    /// `CatalogSnapshot`. Mirror of [`Engine::prepare`] for the
    /// readonly fan-out path: applies the same prepare-time
    /// transforms (clock rewrite, `GROUP BY ALL` expansion, ORDER
    /// BY position resolve, cost-based JOIN reorder) but resolves
    /// catalog + statistics against the snapshot, not a live
    /// engine. Static-on-Self — `AsyncReadHandle::prepare` calls
    /// this without taking the writer lock so multiple read
    /// handles can prepare concurrently against frozen views.
    ///
    /// # Errors
    /// Propagates [`ParseError`] from the parser. Schema
    /// validation deferred to execute time, same as
    /// [`Engine::prepare`].
    pub fn prepare_on_snapshot(
        snapshot: &CatalogSnapshot,
        sql: &str,
    ) -> Result<Statement, ParseError> {
        let mut stmt = parser::parse_statement(sql)?;
        let now_micros = snapshot.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            expand_group_by_all(s);
            resolve_order_by_position(s);
            reorder::reorder_joins(s, &snapshot.catalog, &snapshot.statistics);
        }
        Ok(stmt)
    }

    /// **v4.0 concurrency**: this is the entry point the server takes
    /// under an `RwLock::read()` so multiple `SELECT` clients run in
    /// parallel without serialising on a single mutex.
    pub fn execute_readonly(&self, sql: &str) -> Result<QueryResult, EngineError> {
        self.execute_readonly_with_cancel(sql, CancelToken::none())
    }

    /// v4.5 — read path with cooperative cancellation. Token's
    /// `is_cancelled` is checked at the start (so a watchdog that
    /// already fired returns Cancelled immediately) and at row-loop
    /// checkpoints inside `exec_select`. SHOW paths are O(small) and
    /// don't bother checking.
    pub fn execute_readonly_with_cancel(
        &self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut stmt = parser::parse_statement_with(sql, self.backslash_escapes)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
            // v6.2.3 — cost-based JOIN reorder (read path).
            reorder::reorder_joins(s, &self.catalog, &self.statistics);
        }
        self.execute_readonly_stmt_with_cancel(stmt, cancel)
    }

    /// v7.18 — readonly dispatch on a pre-parsed `Statement`.
    /// Internal helper shared by the SQL-string path
    /// ([`Engine::execute_readonly_with_cancel`]) and the prepared-
    /// statement path ([`Engine::execute_readonly_prepared_on_snapshot_with_cancel`]).
    /// Statement-level transforms (clock rewrite, ORDER BY position,
    /// JOIN reorder, placeholder substitution) are the caller's
    /// responsibility — this helper assumes the AST is already
    /// execution-ready. Writes / DDL hit
    /// [`EngineError::WriteRequired`] the same way the SQL path does.
    fn execute_readonly_stmt_with_cancel(
        &self,
        stmt: Statement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let result = match stmt {
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
            Statement::ShowTables => Ok(self.exec_show_tables()),
            Statement::ShowDatabases => Ok(self.exec_show_databases()),
            Statement::ShowCreateTable(name) => self.exec_show_create_table(&name),
            Statement::ShowIndexes(name) => self.exec_show_indexes(&name),
            Statement::ShowStatus => Ok(self.exec_show_status()),
            Statement::ShowVariables => Ok(self.exec_show_variables()),
            Statement::ShowProcesslist => Ok(self.exec_show_processlist()),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            Statement::ShowPublications => Ok(self.exec_show_publications()),
            Statement::ShowSubscriptions => Ok(self.exec_show_subscriptions()),
            Statement::WaitForWalPosition { .. } => Err(EngineError::Unsupported(
                "WAIT FOR WAL POSITION must be handled by the server layer".into(),
            )),
            Statement::Explain(e) => self.exec_explain(&e, cancel),
            _ => Err(EngineError::WriteRequired),
        };
        self.enforce_row_limit(result)
    }
}
