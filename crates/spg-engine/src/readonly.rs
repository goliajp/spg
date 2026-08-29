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
        params: &[Value<'static>],
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
        params: &[Value<'static>],
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
        // A snapshot carries no session, so PG's reading — the stricter
        // one — is the honest default here.
        // A snapshot carries no session, so there is no zone to read the
        // local-clock family in — UTC, as before.
        rewrite_clock_calls(&mut stmt, now_micros, false, 0);
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

    /// v7.37.x (SPGS PROJ wire encode tax) — read-path streaming
    /// SELECT. Parses the SQL, applies the same statement-level
    /// rewrites the read path does (`rewrite_clock_calls`,
    /// `resolve_order_by_position`, `reorder::reorder_joins`), then
    /// drives the streaming SELECT executor with the caller's emit
    /// callback. For PROJ-shape SQLs (joined non-aggregate projection
    /// of bound columns over thousands of rows) the engine produces
    /// each row to the emit fn WITHOUT materialising the result into
    /// `Vec<Row<'static>>` — the per-cell `.cloned()` and per-row
    /// `Row::new(values)` disappear. On the 25 k-row PROJ shape
    /// that's about 4 ms saved (one less full result allocation pass
    /// at the engine output boundary).
    ///
    /// Returns the surviving row count emitted (post-WHERE,
    /// post-LIMIT) for the `CommandComplete` tag. Non-SELECT
    /// statements surface as `Unsupported` so the caller can fall
    /// back to the materialising read path.
    /// v7.37.x (docker-fair SCALARSQ wire-overhead attack) — prepared-
    /// SelectStatement variant. Caller has already run
    /// `parser::parse_statement_with` + `rewrite_clock_calls` +
    /// `resolve_order_by_position` + `reorder::reorder_joins` (the
    /// per-connection parse cache in spg-server's pgwire layer caches
    /// the post-prepare AST and re-applies `rewrite_clock_calls` per
    /// invocation since the clock value embedded in the AST drifts).
    /// Otherwise identical to the SQL-string entry point.
    pub fn prepare_select_streaming(
        &self,
        sql: &str,
    ) -> Result<spg_sql::ast::SelectStatement, EngineError> {
        let mut stmt = parser::parse_statement_with(sql, self.sql_dialect())?;
        // r1043 — the shared pre-pass. This was the third copy of the
        // list; see `Engine::preprocess`.
        self.preprocess(&mut stmt);
        let Statement::Select(s) = stmt else {
            return Err(EngineError::Unsupported(
                "prepare_select_streaming: not a SELECT".into(),
            ));
        };
        Ok(s)
    }

    /// Re-apply `rewrite_clock_calls` to a previously-prepared AST
    /// (cache-friendly: the cached AST's embedded clock literal gets
    /// re-pointed to current time without re-parsing).
    pub fn refresh_clock(&self, s: &mut spg_sql::ast::SelectStatement) {
        let now_micros = self.clock.map(|f| f());
        if now_micros.is_none() {
            return;
        }
        // Wrap as Statement::Select temporarily to reuse the public
        // walker; cheap (one enum tag manipulation).
        let mut stmt = Statement::Select(core::mem::take(s));
        rewrite_clock_calls(
            &mut stmt,
            now_micros,
            self.speaks_mysql,
            now_micros.map_or(0, |n| self.session_tz_offset_at(n)),
        );
        if let Statement::Select(rewritten) = stmt {
            *s = rewritten;
        }
    }

    /// v7.37.x (docker-fair SCALARSQ wire-overhead attack) — prepared
    /// SELECT that returns the full materialised `QueryResult` instead
    /// of driving an emit closure per row. The streaming variant is
    /// only a win when the engine can stream rows lazily (joined
    /// non-aggregate projection through `try_exec_joined_streaming`);
    /// for shapes that materialise inside the engine anyway (anything
    /// with a subquery — including the SCALARSQ shape — and most
    /// aggregates), the emit closure dispatch + cell_refs Vec
    /// management add ~25-50 µs / 100-row response for zero benefit.
    /// This API lets the caller skip the streaming wrapper entirely
    /// and iterate the result rows directly into the wire encoder.
    pub fn execute_readonly_select_prepared(
        &self,
        s: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        self.exec_select_cancel(s, cancel)
    }

    /// v7.37.42-arena Phase 2 — arena-aware streaming SELECT API.
    /// On SCALARSQ streaming-shape detection (`is_scalarsq_streaming_
    /// shape`), routes to `exec_scalarsq_streaming` and emits each
    /// projected row straight out of an arena-backed `bumpalo::Vec`
    /// scratch — no `Vec<Row<'static>>` ever materialises in the
    /// engine for this shape.
    ///
    /// Non-streaming shapes fall through to the generic
    /// `exec_select_cancel` materialised path and emit row-by-row
    /// off the returned `Vec<Row>`; callers stay shape-blind.
    ///
    /// Caller passes a `&'a Bump`; per-row projection scratch lives
    /// in that arena and drops in O(1) at the caller's
    /// `Bump::reset()` / scope end. This is the SPG equivalent of
    /// PG's per-query MessageContext / printtup pattern.
    ///
    /// The shape check is fast (~10 boolean field reads + items
    /// walk); calling on every prepared SELECT is fine.
    pub fn execute_readonly_select_with_arena<'a, F>(
        &self,
        s: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
        arena: &'a bumpalo::Bump,
        mut emit: F,
    ) -> Result<(Vec<spg_storage::ColumnSchema>, usize), EngineError>
    where
        F: FnMut(
            &[spg_storage::ColumnSchema],
            &[spg_storage::Value<'a>],
        ) -> Result<(), EngineError>,
    {
        cancel.check()?;
        // v7.39 (read01 round 57) — this path can short-circuit STRAIGHT into
        // the scalarsq streaming executor, below `exec_select_cancel` and its
        // gate. Check here too.
        self.acl_check_select(s)?;
        // v7.39.2 — and the same for the unknown-column check, for the same
        // reason and found the same way. `SELECT a FROM t WHERE nosuch = 1`
        // over an EMPTY table answered zero rows on the PostgreSQL wire
        // while raising in-process and on the MySQL wire: this path is
        // streamable and `GROUP BY` / `HAVING` are not, so the same
        // statement answered two ways depending on which executor its
        // shape reached.
        self.validate_clause_columns(s)?;
        if crate::scalarsq_streaming::is_scalarsq_streaming_shape(s) {
            return self.exec_scalarsq_streaming(s, cancel, arena, emit);
        }
        // Generic fallback — same as `execute_readonly_select_prepared`
        // but adapted to the streaming-shape API's columns+row
        // callback signature. The arena isn't used here (cells are
        // owned `Value<'static>`); the win for the fallback shape
        // lands in later phases.
        let QueryResult::Rows { columns, rows } = self.exec_select_cancel(s, cancel)? else {
            return Err(EngineError::Unsupported(
                "execute_readonly_select_with_arena fallback got a non-Rows result".into(),
            ));
        };
        for (i, row) in rows.iter().enumerate() {
            // v7.37 (round 824) — the fourth copy of this loop, and the
            // fourth one missing a cancellation check. It cannot share
            // `emit_materialised` because its consumer takes columns and
            // values rather than a `StreamItem`, but it owes the same
            // guarantee: `SELECT id + 0 FROM big` lands here, and under a
            // 120ms timeout it delivered all 200000 rows in 400ms.
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            // `&[Value<'static>]` satisfies `&[Value<'a>]` via
            // covariance of `Cow<'a, str>` in `'a`.
            emit(&columns, &row.values)?;
        }
        let n = rows.len();
        Ok((columns, n))
    }

    pub fn execute_readonly_select_streaming_prepared<F>(
        &self,
        s: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
        mut emit: F,
    ) -> Result<usize, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        cancel.check()?;
        // v7.39 (read01 round 57) — same story: the joined-streaming shortcut
        // runs below `exec_select_cancel`.
        self.acl_check_select(s)?;
        // v7.39.2 — and the unknown-column check, same story again. THIS is
        // the route an autocommit SELECT takes over the PostgreSQL wire,
        // which is why `WHERE nosuch = 1` over an empty table answered
        // zero rows there while raising in-process, on the MySQL wire, and
        // for `GROUP BY` / `HAVING` — shapes this shortcut declines.
        self.validate_clause_columns(s)?;
        if !crate::expr_tree_has_subquery(s)
            && let Some(n) = self.try_exec_joined_streaming(s, cancel, &mut emit)?
        {
            return Ok(n);
        }
        // v7.39 (round 564) — an index-only range emits straight through.
        // Below, the materialising path builds a `Vec<Row>` and this
        // function walks it once to borrow each cell back out; a profile
        // at 50k rows put a fifth of the connection thread's CPU on
        // building and dropping that vector alone.
        if let Some(n) = self.try_index_only_stream(s, &mut emit)? {
            return Ok(n);
        }
        let QueryResult::Rows { columns, rows } = self.exec_select_cancel(s, cancel)? else {
            return Err(EngineError::Unsupported(
                "streaming SELECT got a non-Rows result".into(),
            ));
        };
        crate::execute::emit_materialised(&columns, &rows, cancel, &mut emit)
    }

    pub fn execute_readonly_select_streaming<F>(
        &self,
        sql: &str,
        cancel: CancelToken<'_>,
        mut emit: F,
    ) -> Result<usize, EngineError>
    where
        F: FnMut(crate::StreamItem<'_>) -> Result<(), EngineError>,
    {
        cancel.check()?;
        let mut stmt = parser::parse_statement_with(sql, self.sql_dialect())?;
        // r1043 — the shared pre-pass. THIS is the route every autocommit
        // SELECT takes over the wire, and it was the copy that mattered:
        // `WHERE b = decode(lpad(to_hex(7),16,'0'),'hex')` came through
        // here unfolded and cost 198 ms against 0.013 ms for the same
        // statement's `EXPLAIN ANALYZE` on the same connection, because
        // EXPLAIN went through `prepare` and the query did not.
        self.preprocess(&mut stmt);
        let Statement::Select(s) = stmt else {
            return Err(EngineError::Unsupported(
                "execute_readonly_select_streaming: not a SELECT".into(),
            ));
        };
        // Streaming fast path: joined non-aggregate projection of
        // bound columns. Falls back to the materialising path inside
        // `try_exec_joined_streaming` returning None for any shape
        // that needs the full result (aggregate, ORDER BY, DISTINCT,
        // subqueries, etc.) — the caller's `Vec<Row<'static>>` round-trip
        // still wins because Engine::execute path keeps materialising.
        // v7.39.2 — before the shortcut, for the reason above.
        self.validate_clause_columns(&s)?;
        if !crate::expr_tree_has_subquery(&s)
            && let Some(n) = self.try_exec_joined_streaming(&s, cancel, &mut emit)?
        {
            return Ok(n);
        }
        // Fall back: materialise then iterate. Mirrors the bottom
        // half of `exec_select_streaming` (execute.rs) but at the
        // read path — no `&mut self`, no `current_tx` flip.
        let QueryResult::Rows { columns, rows } = self.exec_select_cancel(&s, cancel)? else {
            return Err(EngineError::Unsupported(
                "streaming SELECT got a non-Rows result".into(),
            ));
        };
        crate::execute::emit_materialised(&columns, &rows, cancel, &mut emit)
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
        let mut stmt = parser::parse_statement_with(sql, self.sql_dialect())?;
        // r1043 — the SAME pre-pass `prepare` runs. This path had its own
        // copy of the list, one pass short of it, and every autocommit
        // SELECT over the wire comes through here: a plan `EXPLAIN`
        // described was not the plan this ran.
        self.preprocess(&mut stmt);
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
        // v7.39 (read01 round 57) — the read path takes the SAME privilege gate
        // as `execute`. Skipping it here would have made every SELECT a way
        // around the ACL: the server dispatches read-only statements down this
        // path, not through `execute`.
        self.acl_check_statement(&stmt)?;
        let result = match stmt {
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
            Statement::ShowTables => Ok(self.exec_show_tables()),
            Statement::ShowDatabases => Ok(self.exec_show_databases()),
            Statement::ShowCreateTable(name) => self.exec_show_create_table(&name),
            Statement::ShowIndexes(name) => self.exec_show_indexes(&name),
            Statement::ShowStatus => Ok(self.exec_show_status()),
            Statement::ShowVariables => Ok(self.exec_show_variables()),
            Statement::ShowVariablesLike(p) => Ok(self.exec_show_variables_like(&p)),
            Statement::ShowProcesslist => Ok(self.exec_show_processlist()),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            // r1058 — the wire routes `SHOW <name>` down the read path;
            // without this arm the canned-inventory fallthrough hit
            // `WriteRequired` ("statement requires a write lock") for
            // `SHOW is_superuser` and friends.
            Statement::ShowParameter(name) => self.exec_show_parameter(name),
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
