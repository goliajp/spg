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
        let saved = self.current_tx;
        self.current_tx = Some(tx_id);
        // v7.34 (crash-recovery P0 #2) — row-level redo capture. Arm the
        // active catalog before dispatch; on success drain the physical
        // changes into `last_redo` for the embedding layer's WAL, on
        // failure discard them (a failed statement leaves no redo; the
        // drain clears the tables' capture buffers either way).
        if self.redo_capture {
            self.active_catalog_mut().enable_redo_all();
        }
        let result = self.execute_inner_with_cancel(sql, cancel);
        if self.redo_capture {
            let drained = self.active_catalog_mut().drain_redo();
            if result.is_ok() {
                self.last_redo = drained;
            }
        }
        self.current_tx = saved;
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
            reorder::reorder_joins(s, &self.catalog, &self.statistics);
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
        params: &[Value],
    ) -> Result<QueryResult, EngineError> {
        self.execute_prepared_with_cancel(stmt, params, CancelToken::none())
    }

    /// v7.17.0 Phase 2.3 — prepared-statement entry that honors a
    /// caller-supplied `CancelToken`. Mirrors `execute_prepared`'s
    /// `current_tx` save/restore so the extended-query path stays
    /// transactionally consistent with the simple-query path.
    pub fn execute_prepared_with_cancel(
        &mut self,
        mut stmt: Statement,
        params: &[Value],
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
        let result = self.execute_stmt_with_cancel(stmt, cancel);
        self.current_tx = saved;
        result
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
        if let (Some(t0), Ok(_)) = (start_us, &result) {
            let now = self.clock.map_or(t0, |f| f());
            let elapsed = now.saturating_sub(t0).max(0) as u64;
            self.query_stats.record(sql, elapsed, now as u64);
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
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
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
