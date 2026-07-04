//! Statement execution + prepared-statement dispatch, split out of
//! `lib.rs` (lib.rs split 17). The public `execute` / `execute_in` /
//! `execute_with_cancel` entry points, the `prepare` / `prepare_cached`
//! / `describe_prepared` / `execute_prepared` prepared-statement path,
//! and the internal pipeline (`execute_inner_with_cancel` →
//! `execute_stmt_with_cancel`) that pre-resolves clock / sequence /
//! placeholder rewrites and routes each parsed Statement to its domain
//! handler (DDL / DML / SELECT / transaction / SHOW / …). Whole
//! `impl Engine` methods reached via the Engine type, so the public
//! surface is unchanged; `execute_stmt_with_cancel` is pub(crate) for
//! the plpgsql + trigger re-entry paths.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::Statement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{ColumnSchema, Value};

use crate::describe;
use crate::{
    CancelToken, Engine, EngineError, IMPLICIT_TX, QueryResult, TxId, expand_group_by_all,
    plan_cache, reorder, resolve_order_by_position, rewrite_clock_calls, substitute_placeholders,
};

/// v7.38 Epic P — turn a caught panic payload into an
/// [`EngineError::Internal`]. Recovers a human-readable detail from the
/// common payload shapes (`&str` / `String`, and the injection framework's
/// typed `InjectedError`) so the wire layer sends a clean message; falls
/// back to a generic string when the payload type is opaque.
#[cfg(feature = "std")]
fn panic_payload_to_engine_error(payload: &(dyn core::any::Any + Send)) -> EngineError {
    // The injection framework panics with a typed error; surface its
    // message so tests get a deterministic, informative string.
    #[cfg(feature = "injection-points")]
    if let Some(inj) = payload.downcast_ref::<crate::testkit::injection::InjectedError>() {
        return EngineError::Internal(alloc::format!("query aborted by internal error: {inj}"));
    }
    let detail = payload
        .downcast_ref::<&'static str>()
        .map(|s| String::from(*s))
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match detail {
        Some(d) => EngineError::Internal(alloc::format!("query aborted by internal error: {d}")),
        None => EngineError::Internal(String::from("query aborted by internal error")),
    }
}

impl Engine {
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        self.execute_in_with_cancel(sql, IMPLICIT_TX, CancelToken::none())
    }

    /// v4.5 — write path with cooperative cancellation. Same dispatch
    /// as `execute_in_with_cancel(sql, IMPLICIT_TX, cancel)`. Kept as
    /// a separate entry point for backward-compat with the v4.5
    /// public API.
    pub fn execute_with_cancel(
        &mut self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.execute_in_with_cancel(sql, IMPLICIT_TX, cancel)
    }

    /// v4.41.1 multi-slot write entry. Routes `sql` through the TX
    /// slot identified by `tx_id` so spg-server dispatch can scope
    /// each implicit-wrap BEGIN..stmt..COMMIT to its own slot in
    /// `tx_catalogs`. `IMPLICIT_TX` is the legacy single-slot path
    /// every other caller (engine self-tests, replay, spg-embedded)
    /// implicitly takes via `execute()` / `execute_with_cancel()`.
    pub fn execute_in(&mut self, sql: &str, tx_id: TxId) -> Result<QueryResult, EngineError> {
        self.execute_in_with_cancel(sql, tx_id, CancelToken::none())
    }

    /// v4.41.1 write path with cooperative cancellation + explicit TX
    /// scope. Sets `self.current_tx` for the duration of the call so
    /// every `exec_*` helper transparently sees its TX's shadow
    /// catalog and savepoint stack; restores on exit so the field is
    /// only valid mid-call (no leakage across calls).
    pub fn execute_in_with_cancel(
        &mut self,
        sql: &str,
        tx_id: TxId,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.38 P0 元机制 A — establish the per-engine injection
        // scope for the duration of this execute. The guard pops
        // the store on drop so nested or sibling engines don't see
        // ours. No-op in release builds (feature off).
        let _inj = self.enter_injection_scope();
        let saved = self.current_tx;
        self.current_tx = Some(tx_id);
        // v7.37.15 (Epic W slice 2) — memoized autocommit writer version
        // is scoped to one statement. Save + reset like `current_tx` so
        // a re-entrant execute (e.g. deferred trigger SQL) can't leak its
        // version into ours, and ours never leaks to the next statement.
        let saved_stmt_wv = self.stmt_writer_version;
        self.stmt_writer_version = None;
        // v7.34 (crash-recovery P0 #2) — row-level redo capture. Arm the
        // active catalog before dispatch; on success drain the physical
        // changes into `last_redo` for the embedding layer's WAL, on
        // failure discard them (a failed statement leaves no redo; the
        // drain clears the tables' capture buffers either way).
        if self.redo_capture {
            self.active_catalog_mut().enable_redo_all();
        }
        // v7.38 Epic P (panic isolation) — run statement execution
        // behind a catch_unwind firewall so a panic in query
        // processing surfaces as an ordinary `EngineError` (after
        // rolling the in-flight tx back) instead of unwinding through
        // the server's engine `RwLock` write guard (which would poison
        // it) or aborting the process. NO-OP under the release
        // `panic = "abort"` profile — the process aborts before any
        // unwind reaches here; active in dev/test (`panic = "unwind"`)
        // and once a later slice flips the release profile.
        let result = self.execute_inner_catching(sql, cancel);
        if self.redo_capture {
            let mut drained = self.active_catalog_mut().drain_redo();
            if result.is_ok() {
                // v7.37.15 (Epic W slice 2) — stamp the real committing
                // writer version onto every change this statement
                // produced. All changes from one statement share the one
                // version (the statement's xmin/xmax): in autocommit it's
                // the memoized value the writes already used; inside an
                // explicit tx it's the deterministic tx entry. Purely
                // additive metadata — replay still resolves by physical
                // position and ignores `writer_version` (later slice).
                if !drained.is_empty() {
                    let v = self.writer_version_for_current_stmt();
                    for change in &mut drained {
                        change.set_writer_version(v);
                    }
                }
                self.last_redo = drained;
            }
        }
        self.current_tx = saved;
        self.stmt_writer_version = saved_stmt_wv;
        result
    }

    /// v6.1.1 — parse and pre-process a SQL string ONCE so the
    /// resulting [`Statement`] can be cached and re-executed via
    /// [`Engine::execute_prepared`]. Returns the same `Statement`
    /// the simple-query path would synthesise internally (clock
    /// rewrites + ORDER BY position-ref resolution applied at
    /// prepare time, since both are session-independent). The
    /// `$N` placeholders in the SQL stay as `Expr::Placeholder(n)`
    /// nodes; they're resolved to concrete values per-call by
    /// `execute_prepared`'s substitution walk.
    ///
    /// Pgwire's `Parse` (P) message lands here.
    pub fn prepare(&self, sql: &str) -> Result<Statement, ParseError> {
        let mut stmt = parser::parse_statement_with(sql, self.backslash_escapes)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            // v6.4.1 — expand `GROUP BY ALL` to every non-aggregate
            // SELECT-list item BEFORE position / alias resolution so
            // downstream passes see the explicit list.
            expand_group_by_all(s);
            resolve_order_by_position(s);
            // v6.2.3 — cost-based JOIN reorder. No-op for
            // single-table FROMs or any non-INNER join shape.
            // v7.38 元机制 D — `SPG_TEST_PLAN_DETERMINISTIC=1` gates
            // this so regression tests pin a stable join order.
            reorder::reorder_joins_with(
                s,
                &self.catalog,
                &self.statistics,
                self.env_cfg.plan_deterministic,
            );
        }
        Ok(stmt)
    }

    /// v6.3.0 — cached prepare. Returns a cloned `Statement` from
    /// the plan cache on hit, runs the full `prepare()` path on miss
    /// and inserts the resulting plan before returning. Skipping the
    /// parse + JOIN-reorder pipeline on hit is the dominant win for
    /// JDBC / sqlx / pgx clients that reuse the same SQL string.
    ///
    /// Returns a cloned `Statement` (not a borrow) because the
    /// pgwire layer owns its `PreparedStmt` map per-session and the
    /// engine-level cache must stay available for other sessions.
    /// Clone cost on a 5-table JOIN AST is well under the parse cost
    /// it replaces.
    pub fn prepare_cached(&mut self, sql: &str) -> Result<Statement, ParseError> {
        // v6.3.1 — version-aware lookup. If the cached plan was
        // prepared before the most recent ANALYZE, evict and replan.
        let current_version = self.statistics.version();
        if let Some(plan) = self.plan_cache.get(sql) {
            if plan.statistics_version == current_version {
                return Ok(plan.stmt.clone());
            }
            // Stale entry — fall through to evict + re-prepare.
        }
        self.plan_cache.evict(sql);
        let stmt = self.prepare(sql)?;
        let source_tables = plan_cache::collect_source_tables(&stmt);
        let plan = plan_cache::PreparedPlan {
            stmt: stmt.clone(),
            statistics_version: current_version,
            source_tables,
            describe_columns: alloc::vec::Vec::new(),
        };
        self.plan_cache.insert(String::from(sql), plan);
        Ok(stmt)
    }

    /// v6.3.0 — read-only accessor for tests and v6.3.1 invalidation.
    pub fn plan_cache(&self) -> &plan_cache::PlanCache {
        &self.plan_cache
    }

    /// v7.38 (mailrs prod 7.35 pool-exhaustion incident) — boot-time
    /// plan-IR cache warm-up. Walks `sqls`, calls `prepare_cached`
    /// on each one. Each successful prepare leaves the parsed +
    /// reordered + clock-rewritten `Statement` in the engine-wide
    /// plan cache; subsequent `Engine::execute` / `execute_prepared`
    /// for the same SQL skips parse + JOIN reorder. Returns the
    /// count of successfully cached statements.
    ///
    /// The mailrs `Database::new` boot path is the expected caller:
    /// pre-warm the top-N query shapes (inbox listing, contacts
    /// search, stats) so the first user-facing request doesn't
    /// pay the 2-3 s first-fire cost on the readonly-blocking
    /// sqlx pool — which (under prod concurrency) exhausts the
    /// pool and stalls the whole UI.
    pub fn warm_up_plan_cache(&mut self, sqls: &[&str]) -> usize {
        let mut warmed = 0;
        for sql in sqls {
            if self.prepare_cached(sql).is_ok() {
                warmed += 1;
            }
        }
        warmed
    }

    /// v7.38 (mailrs prod 7.35 pool-exhaustion incident) — boot-time
    /// cold-tier OS page-cache warm-up. Walks every table in the
    /// active catalog, iterates the cold rows via the existing
    /// BTree-driven `iter_cold_rows_of_table`, drops the rows on
    /// the floor. The walk's side effect is that every cold
    /// segment file gets mmap-read once — the OS page cache then
    /// serves subsequent queries without disk I/O.
    ///
    /// Returns the total cold rows touched across all tables.
    /// On a hot-only catalog (no `cold_segments` populated) the
    /// call is a near-no-op.
    pub fn warm_up_cold_tier(&self) -> usize {
        let catalog = self.active_catalog();
        let mut total = 0;
        for name in catalog.table_names() {
            if let Some(table) = catalog.get(&name) {
                let rows = self.iter_cold_rows_of_table(table);
                total += rows.len();
            }
        }
        total
    }

    /// v6.3.0 — mutable accessor for v6.3.1 invalidation hooks.
    pub fn plan_cache_mut(&mut self) -> &mut plan_cache::PlanCache {
        &mut self.plan_cache
    }

    /// v6.3.3 — Describe a prepared `Statement` without executing.
    /// Returns `(parameter_oids, output_columns)`. Empty
    /// `output_columns` means the statement has no row-producing
    /// shape we could resolve here (JOIN, subquery, non-SELECT, …)
    /// — pgwire layer maps that to a `NoData` reply.
    pub fn describe_prepared(&self, stmt: &Statement) -> (Vec<u32>, Vec<ColumnSchema>) {
        describe::describe_prepared(stmt, self.active_catalog())
    }

    /// v6.1.1 — execute a [`Statement`] previously returned by
    /// [`Engine::prepare`], substituting `Expr::Placeholder(n)`
    /// nodes for the corresponding [`Value`] in `params` (1-based
    /// per PG: `$1` → `params[0]`). Bind-time string parameters
    /// are decoded into typed `Value`s by the pgwire layer before
    /// this call so the resulting AST hits the same execution
    /// path as a simple query — no SQL re-parse.
    ///
    /// Pgwire's `Execute` (E) message after a `Bind` (B) lands here.
    pub fn execute_prepared(
        &mut self,
        stmt: Statement,
        params: &[Value<'static>],
    ) -> Result<QueryResult, EngineError> {
        self.execute_prepared_with_cancel(stmt, params, CancelToken::none())
    }

    /// v7.37 (SPGS small-query bar) — borrow-based SELECT entry for
    /// the pgwire `Execute` hot path when the portal has no bound
    /// parameters. Skips both the AST clone the prepared path used
    /// to do at the pgwire call site AND the `substitute_
    /// placeholders` walk (a no-op when params are empty). Caller
    /// must already hold the engine write lock — read would be
    /// cleaner, but `current_tx` mutation keeps it `&mut`.
    pub fn execute_prepared_select_no_params(
        &mut self,
        stmt: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let saved = self.current_tx;
        self.current_tx = Some(IMPLICIT_TX);
        let result = self.exec_select_cancel(stmt, cancel);
        self.current_tx = saved;
        result
    }

    /// v7.37 — streaming SELECT for the pgwire `Execute` hot path.
    /// Emits one `StreamItem::Header(cols)` then one
    /// `StreamItem::Row(&[&Value])` per surviving row. Returns the
    /// total row count for the `CommandComplete` tag.
    ///
    /// For shapes where the engine can stream directly (non-aggregate
    /// join projection of bound columns, no ORDER BY / DISTINCT / etc.)
    /// no `Vec<Row<'static>>` is materialised — cell references come straight
    /// out of the source tables. For non-streamable shapes the engine
    /// runs the full `exec_select_cancel`, then walks the materialised
    /// `Vec<Row<'static>>` driving the same emit callback (no engine-side win,
    /// but pgwire dispatches every Execute through one path).
    pub fn execute_prepared_select_streaming<F>(
        &mut self,
        stmt: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
        mut emit: F,
    ) -> Result<usize, EngineError>
    where
        F: FnMut(StreamItem<'_>) -> Result<(), EngineError>,
    {
        let saved = self.current_tx;
        self.current_tx = Some(IMPLICIT_TX);
        let inner = self.exec_select_streaming(stmt, cancel, &mut emit);
        self.current_tx = saved;
        inner
    }

    /// v7.37 — internal streaming dispatcher. Phase 1: fall-back path
    /// only — runs the materialising `exec_select_cancel`, then drives
    /// the emit callback from the resulting `Vec<Row<'static>>`. Phase 2 will
    /// add a true streaming path for the joined-projection shape.
    fn exec_select_streaming<F>(
        &mut self,
        stmt: &spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
        emit: &mut F,
    ) -> Result<usize, EngineError>
    where
        F: FnMut(StreamItem<'_>) -> Result<(), EngineError>,
    {
        // v7.37 — true-streaming fast path for joined-non-aggregate
        // projection of bound columns. Skips `Vec<Row<'static>>` + per-cell
        // `.cloned()` (about 4 ms saved on the 25 k-row PROJ shape).
        // Unresolved subqueries / pull-up shapes / non-streamable
        // structure (ORDER BY, DISTINCT, …) fall through to the
        // materialising path.
        if !crate::subquery::expr_tree_has_subquery(stmt) {
            if let Some(n) = self.try_exec_joined_streaming(stmt, cancel, emit)? {
                return Ok(n);
            }
        }
        // Fall-back: materialise then iterate.
        let QueryResult::Rows { columns, rows } = self.exec_select_cancel(stmt, cancel)? else {
            return Err(EngineError::Unsupported(alloc::string::String::from(
                "streaming SELECT got a non-Rows result",
            )));
        };
        emit(StreamItem::Header(&columns))?;
        let mut cell_refs: Vec<&Value> = Vec::with_capacity(columns.len());
        for row in &rows {
            cell_refs.clear();
            for v in &row.values {
                cell_refs.push(v);
            }
            emit(StreamItem::Row(&cell_refs))?;
        }
        Ok(rows.len())
    }
}

/// v7.37 — one item in the streaming SELECT emit channel. The
/// engine yields exactly one `Header` (before any row) then zero
/// or more `Row`s. Pgwire (or any other consumer) decides how to
/// turn those into wire bytes.
#[derive(Debug)]
pub enum StreamItem<'a> {
    Header(&'a [ColumnSchema]),
    Row(&'a [&'a Value<'static>]),
}

impl Engine {
    /// v7.17.0 Phase 2.3 — prepared-statement entry that honors a
    /// caller-supplied `CancelToken`. Mirrors `execute_prepared`'s
    /// `current_tx` save/restore so the extended-query path stays
    /// transactionally consistent with the simple-query path.
    pub fn execute_prepared_with_cancel(
        &mut self,
        mut stmt: Statement,
        params: &[Value<'static>],
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        substitute_placeholders(&mut stmt, params)?;
        // v7.16.0 — set `current_tx` for the duration of the
        // dispatch so the `exec_*` helpers see the right TX
        // slot (matches what `execute_in_with_cancel` does for
        // simple-query). Pre-v7.16 the simple-query path
        // worked because every public entry point routed
        // through `execute_in_with_cancel`; the prepared path
        // skipped the wrap and so its INSERTs/UPDATEs landed
        // in the no-tx default slot, silently invisible to a
        // BEGIN/COMMIT-bracketed flow. Caught by spg-sqlx's
        // first transaction-visibility test.
        let saved = self.current_tx;
        self.current_tx = Some(IMPLICIT_TX);
        // v7.38 Epic P (panic isolation) — Slice 2: route the
        // prepared / extended-query path (the one sqlx / asyncpg / most
        // drivers actually use via pgwire `Bind`+`Execute`) through the
        // SAME `catch_unwind` firewall as the simple-query path (Slice 1,
        // `execute_inner_catching`). A panic in an extended-protocol
        // statement is caught inside the engine, the in-flight tx is
        // rolled back (shared `discard_tx_on_panic`), and the caller sees
        // an ordinary `EngineError::Internal` — never a poisoned write
        // guard or an aborted process. `current_tx` is `Some(IMPLICIT_TX)`
        // for the duration, so a caught panic rolls back the right tx; the
        // `saved` restore below still runs because the catch converts the
        // unwind into a normal `Result` return.
        let result = self.execute_stmt_catching(stmt, cancel);
        self.current_tx = saved;
        result
    }

    /// v7.38 Epic P (panic isolation) — shared `catch_unwind` firewall
    /// (hosted `std` builds) used by both the simple-query
    /// ([`Self::execute_inner_catching`]) and the prepared / extended-query
    /// ([`Self::execute_stmt_catching`]) entry paths. Runs `run` under
    /// `catch_unwind`; a panic that unwinds out of statement execution is
    /// caught here and converted to [`EngineError::Internal`] after
    /// discarding the in-flight tx's shadow, so the caller sees a normal SQL
    /// error and the engine stays alive. This is the single place the
    /// rollback-on-panic policy lives — neither entry path reimplements it.
    ///
    /// **Why the post-catch engine state is consistent (COW shadow argument):**
    /// every uncommitted write of the panicked statement lives in
    /// `tx_catalogs[current_tx].catalog` — a per-tx *shadow* catalog that is
    /// only merged into the committed `self.catalog` at COMMIT (see
    /// `exec_commit`). The committed catalog is therefore never touched
    /// mid-statement, so dropping the shadow (mirroring `exec_rollback`)
    /// discards all half-applied work and leaves `self.catalog` exactly as it
    /// was before the statement. Redo-capture buffers live inside the
    /// shadow's tables and die with it, so no partial `RowChange` leaks into
    /// `last_redo` either (the caller publishes `last_redo` only on `Ok`).
    ///
    /// The `catch_unwind` closure holds `&mut self`; wrapping it in
    /// `AssertUnwindSafe` is sound precisely because of the above — the only
    /// caller-visible state a caught panic can leave behind is the discarded
    /// shadow, which is the correct rollback outcome, not a torn invariant.
    #[cfg(feature = "std")]
    fn catch_stmt_panic(
        &mut self,
        run: impl FnOnce(&mut Self) -> Result<QueryResult, EngineError>,
    ) -> Result<QueryResult, EngineError> {
        extern crate std;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(self)));
        match caught {
            Ok(result) => result,
            Err(payload) => {
                // The panic unwound past every `?`-return in the executor.
                // `current_tx` is Some here (set by the caller); roll that tx
                // back by discarding its shadow. A statement that panicked in
                // autocommit before any shadow was opened simply has nothing
                // to drop (`discard_tx_on_panic` is infallible).
                let tx_id = self.current_tx.unwrap_or(IMPLICIT_TX);
                self.discard_tx_on_panic(tx_id);
                Err(panic_payload_to_engine_error(payload.as_ref()))
            }
        }
    }

    /// v7.38 Epic P (panic isolation) — simple-query path wrapper: run
    /// [`Self::execute_inner_with_cancel`] behind the shared
    /// [`Self::catch_stmt_panic`] firewall.
    #[cfg(feature = "std")]
    fn execute_inner_catching(
        &mut self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.catch_stmt_panic(|s| s.execute_inner_with_cancel(sql, cancel))
    }

    /// `no_std` variant — there is no unwinding runtime, so statement
    /// execution runs directly with no catch.
    #[cfg(not(feature = "std"))]
    fn execute_inner_catching(
        &mut self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.execute_inner_with_cancel(sql, cancel)
    }

    /// v7.38 Epic P (panic isolation) — Slice 2: prepared / extended-query
    /// path wrapper. The extended-protocol path already holds a resolved
    /// [`Statement`] (no re-parse), so it cannot reuse the `&str`-taking
    /// [`Self::execute_inner_catching`]; instead it runs
    /// [`Self::execute_stmt_with_cancel`] behind the SAME shared
    /// [`Self::catch_stmt_panic`] firewall — identical rollback + error
    /// semantics, zero duplicated policy.
    #[cfg(feature = "std")]
    fn execute_stmt_catching(
        &mut self,
        stmt: Statement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.catch_stmt_panic(|s| s.execute_stmt_with_cancel(stmt, cancel))
    }

    /// `no_std` variant — there is no unwinding runtime, so statement
    /// execution runs directly with no catch.
    #[cfg(not(feature = "std"))]
    fn execute_stmt_catching(
        &mut self,
        stmt: Statement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.execute_stmt_with_cancel(stmt, cancel)
    }

    /// v7.38 Epic P — discard an in-flight tx's shadow after a caught panic,
    /// mirroring the state cleanup of [`Engine::exec_rollback`] but
    /// infallibly. Drops the shadow catalog, marks the tx's writer version
    /// aborted, and releases its row locks. Leaves the committed
    /// `self.catalog` untouched (the COW model kept every uncommitted change
    /// inside the shadow), so this is a full rollback of the panicked
    /// statement's work.
    #[cfg(feature = "std")]
    fn discard_tx_on_panic(&mut self, tx_id: TxId) {
        self.tx_catalogs.remove(&tx_id);
        if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
            self.abort_writer_version(v);
            self.release_tx_locks(v);
        }
        // Per-statement scratch: reset so no stale writer version leaks into
        // the next statement (the caller also restores the saved value).
        self.stmt_writer_version = None;
    }

    fn execute_inner_with_cancel(
        &mut self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let stmt = self.prepare(sql)?;
        // v6.5.1 — wrap the executor with a wall-clock window so we
        // can record into spg_stat_query. Skip when the engine has
        // no clock attached (no_std embedded callers).
        let start_us = self.clock.map(|f| f());
        let result = self.execute_stmt_with_cancel(stmt, cancel);
        if let (Some(t0), Ok(ok)) = (start_us, &result) {
            let now = self.clock.map_or(t0, |f| f());
            let elapsed = now.saturating_sub(t0).max(0) as u64;
            // v7.37.22 (22.9) — count rows produced (SELECT) or
            // affected (INSERT/UPDATE/DELETE) so pg_stat_statements'
            // `rows` column populates accurately.
            let row_count: u64 = match ok {
                QueryResult::Rows { rows, .. } => rows.len() as u64,
                QueryResult::CommandOk { affected, .. } => *affected as u64,
            };
            self.query_stats
                .record_with_rows(sql, elapsed, now as u64, row_count);
            // v6.5.6 — slow-query log: fire callback when elapsed
            // exceeds the configured floor.
            if let (Some(threshold), Some(logger)) =
                (self.slow_query_threshold_us, self.slow_query_logger)
                && elapsed >= threshold
            {
                logger(sql, elapsed);
            }
        }
        result
    }

    pub(crate) fn execute_stmt_with_cancel(
        &mut self,
        stmt: Statement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v7.17.0 Phase 1.1 — pre-resolve nextval / currval /
        // setval calls in the statement tree. Walks SELECT
        // projection, INSERT VALUES, UPDATE SET, DELETE WHERE,
        // and DEFAULT exprs; replaces sequence FunctionCall
        // nodes with concrete Literal values minted against the
        // catalog. This is the only place that mutates sequence
        // state from a SELECT-shaped path (exec_select_cancel is
        // `&self` and can't reach the catalog mutably).
        //
        // Fast-path: when no sequences exist anywhere in the
        // catalog (the typical hot-path INSERT load), skip the
        // walker entirely. Single map-emptiness check on the
        // catalog beats walking every expression on every call.
        let mut stmt = stmt;
        // v7.17 dump-compat — the fast-path check
        // `sequences().is_empty()` skips pre-resolve when no
        // sequence exists in the *currently active* catalog
        // snapshot. The committed catalog or the implicit-TX
        // catalog may legitimately disagree on this between
        // CREATE SEQUENCE and a later setval(): always run the
        // resolver — the walk is O(expr-count) and dwarfed by
        // the parse cost we just paid.
        self.pre_resolve_sequence_calls_in_statement(&mut stmt)?;
        let result = match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            // v7.9.15 — CREATE EXTENSION is a no-op on SPG. Returns
            // CommandOk with affected=0; modified_catalog=false so
            // the WAL doesn't grow a useless entry. mailrs F3.
            Statement::CreateExtension(_) => Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            }),
            // v7.16.2 — DO $$ ... $$ block. mailrs round-10 A.2
            // — the pre-v7.9.27 no-op SILENTLY swallowed every
            // mailrs migrate-038/-040/-042 idempotent rename
            // (the IF EXISTS … THEN ALTER … END block never
            // ran). v7.16.2 dispatches to exec_do_block which
            // runs the PlPgSqlBlock at top level via the same
            // execute_stmts machinery the trigger executor
            // uses (NEW=None, OLD=None — DO blocks have no
            // row context).
            Statement::DoBlock(body) => self.exec_do_block(body),
            // v7.14.0 — empty-statement no-op for pg_dump /
            // mysqldump preamble lines that collapse to nothing
            // after comment-stripping.
            Statement::Empty => Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            }),
            Statement::DropTable { names, if_exists } => self.exec_drop_table(names, if_exists),
            Statement::DropIndex { name, if_exists } => self.exec_drop_index(name, if_exists),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Update(mut s) => {
                // Materialise uncorrelated subqueries in SET / WHERE
                // before the row walk — the SELECT path has done this
                // since v4.10; UPDATE gained it for mailrs's
                // `UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP
                // LOCKED)` claim pattern (embed round-12).
                for (_, e) in &mut s.assignments {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                if let Some(w) = &mut s.where_ {
                    self.resolve_expr_subqueries(w, cancel)?;
                }
                self.exec_update_cancel(&s, cancel)
            }
            Statement::Delete(mut s) => {
                if let Some(w) = &mut s.where_ {
                    self.resolve_expr_subqueries(w, cancel)?;
                }
                self.exec_delete_cancel(&s, cancel)
            }
            Statement::Merge(s) => self.exec_merge_cancel(&s, cancel),
            Statement::Select(s) => {
                if s.ctes.iter().any(|c| c.body.is_modifying()) {
                    self.exec_select_with_modifying_ctes(s, cancel)
                } else {
                    self.exec_select_cancel(&s, cancel)
                }
            }
            Statement::CopyTo { table, columns } => {
                self.exec_copy_to(&table, columns.as_deref(), cancel)
            }
            Statement::Begin => self.exec_begin(),
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
            Statement::Savepoint(name) => self.exec_savepoint(name),
            Statement::RollbackToSavepoint(name) => self.exec_rollback_to_savepoint(&name),
            Statement::ReleaseSavepoint(name) => self.exec_release_savepoint(&name),
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
            Statement::CreateUser(s) => self.exec_create_user(&s),
            Statement::DropUser(name) => self.exec_drop_user(&name),
            Statement::Explain(e) => self.exec_explain(&e, cancel),
            Statement::AlterIndex(s) => self.exec_alter_index(s),
            Statement::AlterTable(s) => self.exec_alter_table(s),
            Statement::CreatePublication(s) => self.exec_create_publication(s),
            Statement::DropPublication(name) => self.exec_drop_publication(&name),
            Statement::CreateSubscription(s) => self.exec_create_subscription(s),
            Statement::DropSubscription(name) => self.exec_drop_subscription(&name),
            // v6.1.7 — WAIT FOR WAL POSITION needs `lag_state`,
            // which lives in spg-server's ServerState. The engine
            // surfaces a clear error; the server-layer dispatch
            // intercepts the SQL before it reaches the engine on
            // a server build, so this arm only fires for
            // engine-only callers (spg-embedded, lib tests).
            Statement::WaitForWalPosition { .. } => Err(EngineError::Unsupported(
                "WAIT FOR WAL POSITION must be handled by the server layer".into(),
            )),
            // v6.2.0 — ANALYZE recomputes per-column histograms.
            Statement::Analyze(target) => self.exec_analyze(target.as_deref()),
            // v7.37.17 (17.6 sibling) — TRUNCATE [TABLE] <t>[, ...]
            // [RESTART IDENTITY] [CASCADE]. Clears every row from
            // each named table. CASCADE currently accepts the syntax
            // + records the flag; the FK-referring cascade walk lands
            // when FK-cascade delete surface gets extended to
            // multi-relation batching (v7.38).
            Statement::Truncate {
                tables,
                restart_identity,
                cascade: _,
            } => self.exec_truncate(tables.as_slice(), restart_identity),
            // v6.7.3 — COMPACT COLD SEGMENTS.
            Statement::CompactColdSegments => self.exec_compact_cold_segments(),
            // v7.12.1 — SET / RESET session parameter. Engine
            // tracks the value in `session_params`; FTS dispatcher
            // reads `default_text_search_config`. Everything else
            // is a recorded no-op (PG dump compat).
            Statement::SetParameter { name, value } => {
                self.set_session_param(name, value);
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            // v7.38 轴 4 — `SET TRANSACTION ISOLATION LEVEL …`. The
            // surface is recorded on `Engine::current_isolation_level`
            // and visible via `SHOW transaction_isolation`. Behavioural
            // implementation (REPEATABLE READ snapshot / SERIALIZABLE
            // SSI) lands separately; today every level reads as
            // effective READ COMMITTED (same as PG's silent upgrade
            // of READ UNCOMMITTED).
            Statement::SetTransaction { isolation } => {
                self.current_isolation_level = isolation;
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            // v7.38 轴 4 surface expansion — `SHOW <parameter>`
            // returns a 1-row 1-column TEXT result (the PG psql
            // wire shape). The handler dispatches per-name:
            //
            // 1. transaction_isolation — direct read of
            //    current_isolation_level (the v7.38 axis-4 surface).
            // 2. PG preset / engine-tracked params — values mirror
            //    pg_catalog.pg_settings to keep ORM /
            //    driver-connect probes happy (sqlx asks
            //    server_version + standard_conforming_strings +
            //    client_encoding; npgsql asks application_name;
            //    asyncpg asks search_path). Any
            //    SET-tracked override on self.session_params wins.
            // 3. Anything else — error with a list-pointer to
            //    pg_settings (which lists every recognised name).
            Statement::ShowParameter(name) => {
                use spg_storage::{ColumnSchema, DataType, Row, Value};
                // v7.37.17 (17.6 sibling) — `SHOW ALL` returns a
                // (name, setting, description) triple for every
                // parameter SPG knows about. PG's shape is the same.
                // Emitting a fixed curated inventory here keeps the
                // client shape stable without wire-tapping every
                // per-session parameter.
                if name.eq_ignore_ascii_case("all") {
                    let params: &[(&str, &str, &str)] = &[
                        ("server_version", "16.0 (spg)", "Reports the server version."),
                        ("server_encoding", "UTF8", "Sets the server's encoding."),
                        ("client_encoding", "UTF8", "Sets the client's encoding."),
                        ("is_superuser", "on", "Reports superuser status."),
                        ("TimeZone", "UTC", "Sets the time zone for displaying and interpreting timestamps."),
                        ("DateStyle", "ISO, MDY", "Sets the display format for date/time values."),
                        ("IntervalStyle", "postgres", "Sets the display format for interval values."),
                        ("search_path", "\"$user\", public", "Sets the schema search order."),
                        ("standard_conforming_strings", "on", "Causes '...' strings to treat backslashes literally."),
                        ("statement_timeout", "0", "Sets the maximum allowed duration of any statement."),
                        ("application_name", "", "Sets the application name for status displays."),
                        ("transaction_isolation", self.current_isolation_level.as_pg_str(), "Sets the current transaction's isolation level."),
                        ("default_transaction_isolation", "read committed", "Sets the transaction isolation level of each new transaction."),
                    ];
                    let cols = alloc::vec![
                        ColumnSchema::new("name", DataType::Text, false),
                        ColumnSchema::new("setting", DataType::Text, false),
                        ColumnSchema::new("description", DataType::Text, false),
                    ];
                    let rows: Vec<Row> = params
                        .iter()
                        .map(|(n, v, d)| {
                            Row::new(alloc::vec![
                                Value::text(alloc::string::String::from(*n)),
                                Value::text(alloc::string::String::from(*v)),
                                Value::text(alloc::string::String::from(*d)),
                            ])
                        })
                        .collect();
                    return Ok(QueryResult::Rows { columns: cols, rows });
                }
                let owned;
                let value: &str = match name.as_str() {
                    "transaction_isolation" => self.current_isolation_level.as_pg_str(),
                    "server_version" => "16.0 (spg)",
                    "server_encoding" => "UTF8",
                    "is_superuser" => "on",
                    "TimeZone" | "timezone" => self
                        .session_param("TimeZone")
                        .or_else(|| self.session_param("timezone"))
                        .unwrap_or("UTC"),
                    "DateStyle" | "datestyle" => self
                        .session_param("DateStyle")
                        .or_else(|| self.session_param("datestyle"))
                        .unwrap_or("ISO, MDY"),
                    "client_encoding" => self.session_param("client_encoding").unwrap_or("UTF8"),
                    "standard_conforming_strings" => self
                        .session_param("standard_conforming_strings")
                        .unwrap_or("on"),
                    "search_path" => self
                        .session_param("search_path")
                        .unwrap_or("\"$user\", public"),
                    "application_name" => self.session_param("application_name").unwrap_or(""),
                    "statement_timeout" => self.session_param("statement_timeout").unwrap_or("0"),
                    "default_transaction_isolation" => self
                        .session_param("default_transaction_isolation")
                        .unwrap_or("read committed"),
                    "intervalstyle" | "IntervalStyle" => self
                        .session_param("IntervalStyle")
                        .or_else(|| self.session_param("intervalstyle"))
                        .unwrap_or("postgres"),
                    // v7.37.17 (17.6 siblings) — PG defaults for
                    // parameters SPG doesn't yet honor at the engine
                    // level. Drivers probe these with SHOW right
                    // after a matching SET to verify the SET worked;
                    // reporting the PG default keeps drivers happy
                    // without having to plumb every param into
                    // engine state.
                    "lock_timeout" => self.session_param("lock_timeout").unwrap_or("0"),
                    "idle_in_transaction_session_timeout" => self
                        .session_param("idle_in_transaction_session_timeout")
                        .unwrap_or("0"),
                    "transaction_timeout" => self
                        .session_param("transaction_timeout")
                        .unwrap_or("0"),
                    "client_min_messages" => self
                        .session_param("client_min_messages")
                        .unwrap_or("notice"),
                    "default_tablespace" => self
                        .session_param("default_tablespace")
                        .unwrap_or(""),
                    "default_table_access_method" => self
                        .session_param("default_table_access_method")
                        .unwrap_or("heap"),
                    "row_security" => self.session_param("row_security").unwrap_or("on"),
                    "check_function_bodies" => self
                        .session_param("check_function_bodies")
                        .unwrap_or("on"),
                    "xmloption" => self.session_param("xmloption").unwrap_or("content"),
                    "work_mem" => self.session_param("work_mem").unwrap_or("4MB"),
                    "maintenance_work_mem" => self
                        .session_param("maintenance_work_mem")
                        .unwrap_or("64MB"),
                    "max_connections" => {
                        self.session_param("max_connections").unwrap_or("100")
                    }
                    "shared_buffers" => {
                        self.session_param("shared_buffers").unwrap_or("128MB")
                    }
                    "effective_cache_size" => self
                        .session_param("effective_cache_size")
                        .unwrap_or("4GB"),
                    other => {
                        // Fall through to session_params for any user-set
                        // override that didn't fall into a named bucket.
                        if let Some(v) = self.session_param(other) {
                            owned = alloc::string::String::from(v);
                            &owned
                        } else {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "SHOW {other:?}: parameter not recognised; \
                                 see `SELECT name, setting FROM pg_settings` for \
                                 the full inventory"
                            )));
                        }
                    }
                };
                Ok(QueryResult::Rows {
                    columns: alloc::vec![ColumnSchema::new(name, DataType::Text, false)],
                    rows: alloc::vec![Row::new(alloc::vec![Value::text(value)])],
                })
            }
            // v7.14.0 — MySQL multi-assignment SET. Each pair runs
            // through `set_session_param` so engine-known params
            // (FOREIGN_KEY_CHECKS, session_replication_role, …) take
            // effect; unknown pairs (including `@VAR` LHS from the
            // mysqldump preamble) are recorded then ignored.
            Statement::SetParameterList(pairs) => {
                for (name, value) in pairs {
                    self.set_session_param(name, value);
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            // v7.12.4 — CREATE FUNCTION / CREATE TRIGGER / DROP …
            // for the PL/pgSQL trigger surface. exec_* methods are
            // defined alongside the existing CREATE handlers below.
            Statement::CreateFunction(s) => self.exec_create_function(s),
            Statement::CreateTrigger(s) => self.exec_create_trigger(s),
            Statement::DropTrigger {
                name,
                table,
                if_exists,
            } => self.exec_drop_trigger(&name, &table, if_exists),
            Statement::DropFunction { name, if_exists } => {
                self.exec_drop_function(&name, if_exists)
            }
            Statement::CreateSequence(s) => self.exec_create_sequence(s),
            Statement::AlterSequence(s) => self.exec_alter_sequence(s),
            Statement::DropSequence { names, if_exists } => {
                self.exec_drop_sequence(&names, if_exists)
            }
            Statement::CreateView(s) => self.exec_create_view(s),
            Statement::DropView { names, if_exists } => self.exec_drop_view(&names, if_exists),
            Statement::CreateMaterializedView(s) => self.exec_create_materialized_view(s),
            Statement::RefreshMaterializedView { name, with_data } => {
                self.exec_refresh_materialized_view(&name, with_data)
            }
            Statement::DropMaterializedView { names, if_exists } => {
                self.exec_drop_materialized_view(&names, if_exists)
            }
            Statement::CreateType(s) => self.exec_create_type(s),
            Statement::DropType { names, if_exists } => self.exec_drop_type(&names, if_exists),
            Statement::CreateDomain(s) => self.exec_create_domain(s),
            Statement::DropDomain { names, if_exists } => self.exec_drop_domain(&names, if_exists),
            Statement::CreateSchema {
                name,
                if_not_exists,
            } => self.exec_create_schema(name, if_not_exists),
            Statement::DropSchema { names, if_exists } => self.exec_drop_schema(&names, if_exists),
            Statement::ResetParameter(target) => {
                match target {
                    None => self.session_params.clear(),
                    Some(name) => {
                        self.session_params.remove(&name.to_ascii_lowercase());
                    }
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
        };
        self.enforce_row_limit(result)
    }
}

impl Engine {
    /// `COPY table [(cols)] TO STDOUT` — render the visible rows
    /// in COPY text format (tab-separated, `\N` nulls, backslash
    /// escapes) as a single-text-column result set. Embedded
    /// consumers read the lines directly; the wire layer streams
    /// CopyData frames from them.
    fn exec_copy_to(
        &mut self,
        table_name: &str,
        columns: Option<&[String]>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let table = self.active_catalog().get(table_name).ok_or_else(|| {
            EngineError::Storage(spg_storage::StorageError::TableNotFound {
                name: alloc::string::String::from(table_name),
            })
        })?;
        let schema_cols = table.schema().columns.clone();
        let positions: alloc::vec::Vec<usize> = match columns {
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema_cols
                        .iter()
                        .position(|s| s.name.eq_ignore_ascii_case(c))
                        .ok_or_else(|| {
                            EngineError::Eval(crate::eval::EvalError::ColumnNotFound {
                                name: c.clone(),
                            })
                        })
                })
                .collect::<Result<_, _>>()?,
            None => (0..schema_cols.len()).collect(),
        };
        let snap = self.current_snapshot();
        let mut out_rows: alloc::vec::Vec<spg_storage::Row<'static>> = alloc::vec::Vec::new();
        let encode = |row: &spg_storage::Row<'static>| {
            let cells: alloc::vec::Vec<Option<alloc::string::String>> = positions
                .iter()
                .map(|&p| match row.values.get(p) {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(crate::eval::values::value_to_text(v)),
                })
                .collect();
            crate::copy::encode_copy_text_cells(&cells)
        };
        for (_, row) in table.scan_visible(&snap) {
            cancel.check()?;
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(encode(row))]));
        }
        for row in self.iter_cold_rows_of_table(table) {
            cancel.check()?;
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(encode(&row))]));
        }
        Ok(QueryResult::Rows {
            columns: alloc::vec![spg_storage::ColumnSchema::new(
                alloc::string::String::from("copy"),
                spg_storage::DataType::Text,
                false,
            )],
            rows: out_rows,
        })
    }
}
