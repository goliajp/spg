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
/// v7.38 (read01 P3.17) — reject a clearly-invalid value for a handful of
/// well-known typed GUCs (boolean / memory-size / duration), so a typo
/// like `SET work_mem = 'bogus'` errors like PG instead of silently
/// storing junk. Conservative: only GUCs whose type is unambiguous are
/// checked; every other name is accepted so pg_dump preambles and
/// unknown settings still load.
fn validate_known_guc(name: &str, value: &str) -> Result<(), EngineError> {
    let key = name.to_ascii_lowercase();
    let bad = || {
        EngineError::Unsupported(alloc::format!(
            "invalid value for parameter \"{name}\": \"{value}\""
        ))
    };
    let is_bool = matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "on" | "off" | "true" | "false" | "yes" | "no" | "1" | "0"
    );
    // Split a `<number><unit>` GUC value into its numeric head + unit tail.
    let split_unit = |s: &str| -> (String, String) {
        let st = s.trim();
        let cut = st
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(st.len());
        (
            String::from(st[..cut].trim()),
            st[cut..].trim().to_ascii_lowercase(),
        )
    };
    let (num, unit) = split_unit(value);
    let is_size =
        num.parse::<f64>().is_ok() && matches!(unit.as_str(), "" | "b" | "kb" | "mb" | "gb" | "tb");
    let is_duration = num.parse::<i64>().is_ok()
        && matches!(unit.as_str(), "" | "us" | "ms" | "s" | "min" | "h" | "d");
    match key.as_str() {
        "enable_seqscan"
        | "enable_indexscan"
        | "enable_bitmapscan"
        | "enable_indexonlyscan"
        | "enable_hashjoin"
        | "enable_mergejoin"
        | "enable_nestloop"
        | "autovacuum"
        | "fsync"
        | "full_page_writes" => {
            if !is_bool {
                return Err(bad());
            }
        }
        "work_mem"
        | "maintenance_work_mem"
        | "shared_buffers"
        | "temp_buffers"
        | "effective_cache_size"
        | "wal_buffers" => {
            if !is_size {
                return Err(bad());
            }
        }
        "statement_timeout" | "lock_timeout" | "idle_in_transaction_session_timeout" => {
            if !is_duration {
                return Err(bad());
            }
        }
        // v7.39 (round 171) — synchronous_commit is a real, session-level
        // durability control now (the embedded execute path gates its
        // WAL-fsync wait on it); validate PG's value domain.
        "synchronous_commit" => {
            if !matches!(
                value.to_ascii_lowercase().as_str(),
                "on" | "off" | "local" | "remote_write" | "remote_apply" | "true" | "false"
                    | "0" | "1"
            ) {
                return Err(bad());
            }
        }
        // v7.39 (round 204) — enum GUCs reject an out-of-domain value
        // like PG (`SET client_min_messages = bogus` errors). PG's
        // message quotes the value with a trailing hint listing the
        // valid set; we match the leading, stable clause.
        "client_min_messages" => {
            if !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "debug5" | "debug4" | "debug3" | "debug2" | "debug1" | "log" | "notice"
                    | "warning" | "error" | "fatal" | "panic"
            ) {
                return Err(bad());
            }
        }
        // v7.39 (GUC knife 3) — the render GUCs reject invalid values
        // with PG's own texts (canonical-caps parameter names).
        "datestyle" => {
            if crate::session::parse_datestyle_parts(value, crate::eval::RenderStyle::default())
                .is_none()
            {
                return Err(EngineError::Unsupported(alloc::format!(
                    "invalid value for parameter \"DateStyle\": \"{value}\""
                )));
            }
        }
        "intervalstyle" => {
            if crate::session::parse_intervalstyle(value).is_none() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "invalid value for parameter \"IntervalStyle\": \"{value}\""
                )));
            }
        }
        "extra_float_digits" => match value.trim().parse::<i64>() {
            Ok(n) if (-15..=3).contains(&n) => {}
            Ok(n) => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{n} is outside the valid range for parameter \
                         \"extra_float_digits\" (-15 .. 3)"
                )));
            }
            Err(_) => return Err(bad()),
        },
        _ => {}
    }
    Ok(())
}

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

    /// v7.38 (read01 P3.20) — handle a bare `SELECT set_config(name, value,
    /// is_local)` by writing the GUC to the same session store `SET` uses
    /// (honouring `is_local` via the transaction undo log), so set_config,
    /// SHOW, current_setting, and pg_settings all agree. Returns `None`
    /// (fall through to the ordinary read-only path) unless the statement is
    /// exactly that shape — set_config buried in a FROM/WHERE/CTE, or over a
    /// non-text name, keeps the old value-returning behaviour.
    fn try_exec_set_config(
        &mut self,
        s: &spg_sql::ast::SelectStatement,
    ) -> Result<Option<QueryResult>, EngineError> {
        use spg_sql::ast::{Expr, SelectItem};
        if s.from.is_some() || s.where_.is_some() || !s.ctes.is_empty() || s.items.len() != 1 {
            return Ok(None);
        }
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            return Ok(None);
        };
        let Expr::FunctionCall { name, args } = expr else {
            return Ok(None);
        };
        if !(name.eq_ignore_ascii_case("set_config")
            || name.eq_ignore_ascii_case("pg_catalog.set_config"))
            || !(args.len() == 2 || args.len() == 3)
        {
            return Ok(None);
        }
        // Evaluate the arguments against an empty row.
        let empty: Vec<ColumnSchema> = Vec::new();
        let (name_v, value_v, local_v);
        {
            let ctx = self.ev_ctx(&empty, None);
            let dummy = spg_storage::Row::new(Vec::new());
            name_v = crate::eval::eval_expr(&args[0], &dummy, &ctx).map_err(EngineError::Eval)?;
            value_v = crate::eval::eval_expr(&args[1], &dummy, &ctx).map_err(EngineError::Eval)?;
            local_v = if args.len() == 3 {
                crate::eval::eval_expr(&args[2], &dummy, &ctx).map_err(EngineError::Eval)?
            } else {
                Value::Bool(false)
            };
        }
        let single = |v: Value<'static>| QueryResult::Rows {
            columns: alloc::vec![ColumnSchema::new(
                "set_config",
                spg_storage::DataType::Text,
                true
            )],
            rows: alloc::vec![spg_storage::Row::new(alloc::vec![v])],
        };
        let pname = match name_v {
            Value::Text(s) => s.into_owned(),
            // set_config(NULL, …) is a no-op returning NULL (PG).
            Value::Null => return Ok(Some(single(Value::Null))),
            _ => return Ok(None),
        };
        let is_local = matches!(local_v, Value::Bool(true));
        // A NULL value resets the GUC to its default (PG), returning NULL.
        let pval = match value_v {
            Value::Text(s) => s.into_owned(),
            Value::Null => {
                self.session_params.remove(&pname.to_ascii_lowercase());
                self.refresh_render_style();
                return Ok(Some(single(Value::Null)));
            }
            _ => return Ok(None),
        };
        validate_known_guc(&pname, &pval)?;
        if is_local {
            if self.in_transaction() {
                let prior = self.session_param(&pname).map(String::from);
                self.local_guc_saves.push((pname.clone(), prior));
                self.set_session_param(pname, spg_sql::ast::SetValue::String(pval.clone()));
            }
        } else {
            self.set_session_param(pname, spg_sql::ast::SetValue::String(pval.clone()));
        }
        Ok(Some(single(Value::text(pval))))
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
        // v7.39 (read01 round 46) — NOTICEs are per-statement: clear the
        // buffer here so one statement's "…, skipping" can never leak into
        // the next one's NoticeResponse batch.
        self.pending_notices.clear();
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
        let pre_in_tx = self.in_transaction();
        let result = self.execute_inner_catching(sql, cancel);
        // v7.39 (round 426) — MySQL's ROW_COUNT() reads what the LAST
        // statement did. Measured on MariaDB 11: a DML statement leaves the
        // number of rows it changed (0 when it matched none), a
        // row-returning statement leaves -1, and DDL leaves 0. One place,
        // because every statement funnels through here — and it must be
        // AFTER the dispatch, so ROW_COUNT()'s own SELECT is what sets -1
        // for the call after it (as MariaDB does).
        //
        // A failed statement leaves the previous value alone: MariaDB keeps
        // the last successful statement's count through an error.
        if let Ok(res) = &result {
            self.row_count = match res {
                QueryResult::CommandOk { affected, .. } => i64::try_from(*affected).unwrap_or(-1),
                QueryResult::Rows { .. } => -1,
            };
        }
        // v7.39 (pg_stat knife A) — PG counts every statement outside a
        // transaction block as one implicit xact (commit on success,
        // rollback on error). Statements INSIDE a block are counted
        // once, by exec_commit / exec_rollback; BEGIN itself (state
        // flips outside -> inside) and the block-closers (inside ->
        // outside, counted in their exec fns) are skipped here.
        if !pre_in_tx && !self.in_transaction() {
            let ctr = if result.is_ok() {
                &self.xact_commit
            } else {
                &self.xact_rollback
            };
            ctr.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        // r196 — a statement that did NOT run inside its own open tx
        // slot (autocommit, or a COMMIT/ROLLBACK that just closed its
        // slot) may have moved the committed base; bump the epoch so
        // OTHER open txs know their next RC rebase is real. The test
        // must be per-statement (`tx_catalogs` membership of THIS
        // call's tx_id), not the global `in_transaction()` — a
        // concurrent autocommit while some tx is open is exactly the
        // case the rebase exists for (the first cut used the global
        // check and 10 isolation pins caught the missed bumps).
        // Deliberately over-approximate (reads bump too — an extra
        // rebase is only slower, never wrong).
        if !self.tx_catalogs.contains_key(&tx_id) {
            self.commit_epoch = self.commit_epoch.wrapping_add(1);
            // v7.39 (round 306) — large-object descriptors live only as
            // long as the transaction that opened them, so this is
            // exactly where they die: an autocommit statement (the
            // implicit transaction just ended) or the COMMIT / ROLLBACK
            // that closed the slot. Numbering restarts from 0, as PG's
            // does. Same per-slot witness as the epoch bump above —
            // another connection's open transaction must not keep this
            // one's descriptors alive.
            self.lo_descriptors.clear();
            self.lo_next_fd = 0;
        }
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
        rewrite_clock_calls(
            &mut stmt,
            now_micros,
            self.backslash_escapes,
            now_micros.map_or(0, |n| self.session_tz_offset_at(n)),
        );
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
    /// v7.39 (round 192) — bump the engine-side per-table DML
    /// counters (pg_stat_user_tables n_tup_*). Non-transactional by
    /// design, like PG's stats collector.
    pub(crate) fn note_table_write(&mut self, table: &str, ins: u64, upd: u64, del: u64) {
        let e = self
            .table_write_stats
            .entry(alloc::string::String::from(table))
            .or_insert((0, 0, 0));
        e.0 = e.0.saturating_add(ins);
        e.1 = e.1.saturating_add(upd);
        e.2 = e.2.saturating_add(del);
    }

    pub fn prepare_cached(&mut self, sql: &str) -> Result<Statement, ParseError> {
        // v7.39 (round 200) — don't cache LARGE statements. A 24 KB
        // multi-row VALUES INSERT paid a full AST deep-clone (~640 µs)
        // just to enter the plan cache, where a unique bulk statement
        // is never reused — and at that size a cache hit would only
        // save the ~190 µs re-parse anyway. The threshold keeps every
        // ORM-shaped statement (small, repeated) on the cached path.
        const PLAN_CACHE_MAX_SQL_BYTES: usize = 4096;
        if sql.len() > PLAN_CACHE_MAX_SQL_BYTES {
            return self.prepare(sql);
        }
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
    /// `output_columns` means the statement has no row-producing shape
    /// we could resolve here — the pgwire layer maps that to `NoData`.
    ///
    /// v7.39 (round 462) — a SELECT over a system catalog view resolves
    /// against the same materialised catalog execution builds, so the
    /// two paths cannot disagree about what a system view looks like.
    pub fn describe_prepared(&self, stmt: &Statement) -> (Vec<u32>, Vec<ColumnSchema>) {
        if let Statement::Select(s) = stmt {
            if crate::system_catalog::select_references_meta_view(s)
                && let Ok(catalog) = self.meta_view_catalog(s)
            {
                return describe::describe_prepared(stmt, &catalog);
            }
            if let Some(catalog) = self.admin_view_catalog(s) {
                return describe::describe_prepared(stmt, &catalog);
            }
        }
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
        // v7.38 Epic P (panic isolation) — Slice 3: route this read-only
        // prepared-SELECT hot path (pgwire `Execute` with no bound params)
        // through the SAME `catch_unwind` firewall as the write paths. A
        // panic in `exec_select_cancel` is caught inside the engine and
        // returned as `EngineError::Internal`, so it never unwinds through
        // the caller's engine `RwLock` write guard (poisoning it) or aborts
        // the process. This path is read-only, so the firewall's
        // `discard_tx_on_panic` is a no-op (no shadow / writer version to
        // drop) — exactly right: nothing to roll back, just catch + survive.
        // `exec_select_cancel` materialises its `QueryResult` synchronously,
        // so the whole result is produced inside the catch (statement
        // boundary only — no per-row cost).
        #[cfg(feature = "std")]
        let result = self.catch_stmt_panic(|s| s.exec_select_cancel(stmt, cancel));
        #[cfg(not(feature = "std"))]
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
        // v7.38 Epic P (panic isolation) — Slice 3: route the streaming
        // read-only SELECT hot path through the SAME `catch_unwind` firewall.
        //
        // Catch SCOPE (verified): `exec_select_streaming` uses a *push*
        // model — it drives the caller's `emit` callback synchronously via
        // `?` for the header and every row (both the true-streaming
        // `try_exec_joined_streaming` fast path and the materialising
        // fall-back), and only returns once the whole result has been
        // emitted. It does NOT hand a lazy iterator back to the wire layer to
        // pull rows from later. Therefore a panic in the per-row streaming
        // phase unwinds *inside* this call and IS caught by wrapping the one
        // `exec_select_streaming` call — the entire streaming phase is
        // covered, not just setup. This is a single statement-boundary catch
        // (the `catch_unwind` landing pad is armed once, the whole emit loop
        // runs inside it) — NOT a per-row catch, so there is no hot-path cost.
        // Read-only, so `discard_tx_on_panic` is a no-op (correct: nothing to
        // roll back). A panic caught mid-stream (after some rows were encoded
        // into the wire buffer) leaves the same partial-`wbuf` + `Err` state
        // the wire layer already handles when `emit` itself returns `Err`
        // mid-stream, so no new torn-state concern is introduced.
        #[cfg(feature = "std")]
        let inner = self.catch_stmt_panic(|s| s.exec_select_streaming(stmt, cancel, &mut emit));
        #[cfg(not(feature = "std"))]
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
    /// v7.39 (round 280) — `CREATE STATISTICS`.
    fn exec_create_statistics(
        &mut self,
        name: String,
        if_not_exists: bool,
        kinds: alloc::vec::Vec<String>,
        columns: alloc::vec::Vec<String>,
        table: String,
    ) -> Result<QueryResult, EngineError> {
        if columns.len() < 2 {
            return Err(EngineError::Unsupported(String::from(
                "extended statistics require at least 2 columns",
            )));
        }
        if self.active_catalog().get(&table).is_none() {
            return Err(EngineError::Unsupported(alloc::format!(
                "relation \"{table}\" does not exist"
            )));
        }
        // PG's default kind set is all three.
        let kinds = if kinds.is_empty() {
            alloc::vec![String::from("d"), String::from("f"), String::from("m")]
        } else {
            kinds
        };
        let def = spg_storage::StatisticsExtDef {
            name: name.clone(),
            table,
            kinds,
            columns,
        };
        let cat = self.active_catalog_mut();
        if let Err(taken) = cat.create_statistics_ext(def) {
            if if_not_exists {
                return Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                });
            }
            return Err(EngineError::Unsupported(alloc::format!(
                "statistics object \"{taken}\" already exists"
            )));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    /// v7.39 (round 280) — `DROP STATISTICS`.
    fn exec_drop_statistics(
        &mut self,
        name: &str,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let dropped = self.active_catalog_mut().drop_statistics_ext(name);
        if !dropped && !if_exists {
            return Err(EngineError::Unsupported(alloc::format!(
                "statistics object \"{name}\" does not exist"
            )));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: dropped,
        })
    }

    /// v7.39 (round 277) — `PREPARE`. Session-scoped, and a duplicate
    /// name is an error in PG rather than a silent replace.
    fn exec_prepare(
        &mut self,
        name: String,
        param_types: alloc::vec::Vec<String>,
        body: Statement,
        source: String,
    ) -> Result<QueryResult, EngineError> {
        if self.prepared_statements.contains_key(&name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "prepared statement \"{name}\" already exists"
            )));
        }
        self.prepared_statements.insert(
            name,
            crate::PreparedSqlStatement {
                body,
                param_types,
                source,
            },
        );
        Ok(QueryResult::CommandOk { affected: 0, modified_catalog: false })
    }

    /// v7.39 (round 277) — `EXECUTE`. The arguments evaluate as
    /// constants and splice into the body's `$N` placeholders through
    /// the same `execute_prepared_with_cancel` the extended-query path
    /// uses, so a SQL EXECUTE and a wire Bind take the identical route.
    fn exec_execute(
        &mut self,
        name: &str,
        args: &[spg_sql::ast::Expr],
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let Some(entry) = self.prepared_statements.get(name) else {
            return Err(EngineError::Unsupported(alloc::format!(
                "prepared statement \"{name}\" does not exist"
            )));
        };
        let body = entry.body.clone();
        let empty: alloc::vec::Vec<spg_storage::ColumnSchema> = alloc::vec::Vec::new();
        let ctx = self.ev_ctx(&empty, None);
        let blank = spg_storage::Row::new(alloc::vec::Vec::new());
        let mut params: alloc::vec::Vec<spg_storage::Value<'static>> =
            alloc::vec::Vec::with_capacity(args.len());
        for a in args {
            params.push(crate::eval::eval_expr(a, &blank, &ctx).map_err(EngineError::Eval)?);
        }
        self.execute_prepared_with_cancel(body, &params, cancel)
    }

    /// v7.39 (round 277) — `DEALLOCATE <name>` / `DEALLOCATE ALL`.
    /// Dropping a name that does not exist is an error in PG; ALL is
    /// unconditional.
    fn exec_deallocate(&mut self, name: Option<&str>) -> Result<QueryResult, EngineError> {
        match name {
            None => {
                self.prepared_statements.clear();
                Ok(QueryResult::CommandOk { affected: 0, modified_catalog: false })
            }
            Some(n) => {
                if self.prepared_statements.remove(n).is_none() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "prepared statement \"{n}\" does not exist"
                    )));
                }
                Ok(QueryResult::CommandOk { affected: 0, modified_catalog: false })
            }
        }
    }

    pub fn execute_prepared_with_cancel(
        &mut self,
        stmt: Statement,
        params: &[Value<'static>],
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        self.execute_prepared_in_with_cancel(stmt, params, IMPLICIT_TX, cancel)
    }

    /// v7.39 (round 303, V22) — like [`Self::execute_prepared_with_cancel`]
    /// but binds the statement to an explicit transaction slot instead of
    /// the implicit one. The mysql-wire binary-protocol path uses this so a
    /// prepared INSERT/UPDATE lands in the connection's own `BEGIN`-opened
    /// transaction (and never collides with another connection on slot 0),
    /// mirroring what pgwire's `Bind`+`Execute` achieves by rendering
    /// bind-final SQL through [`Self::execute_in`].
    pub fn execute_prepared_in(
        &mut self,
        stmt: Statement,
        params: &[Value<'static>],
        tx_id: TxId,
    ) -> Result<QueryResult, EngineError> {
        self.execute_prepared_in_with_cancel(stmt, params, tx_id, CancelToken::none())
    }

    pub fn execute_prepared_in_with_cancel(
        &mut self,
        mut stmt: Statement,
        params: &[Value<'static>],
        tx_id: TxId,
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
        self.current_tx = Some(tx_id);
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
    /// (hosted `std` builds) used by every engine statement entry path: the
    /// simple-query ([`Self::execute_inner_catching`]), the prepared /
    /// extended-query ([`Self::execute_stmt_catching`]), and the read-only
    /// prepared-SELECT hot paths ([`Self::execute_prepared_select_no_params`]
    /// / [`Self::execute_prepared_select_streaming`]). Runs `run` under
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
    ///
    /// Generic over the closure's success type `T` so the read-only
    /// prepared-SELECT paths (which return a row count `usize`, not a
    /// `QueryResult`) reuse the *same* firewall — no second `catch_unwind`
    /// site. On those read-only paths `discard_tx_on_panic` is a no-op (a
    /// SELECT opens no shadow / writer version), which is the correct outcome:
    /// nothing to roll back, the point is purely to catch the unwind, return
    /// `Internal`, and leave the caller's write guard un-poisoned.
    #[cfg(feature = "std")]
    fn catch_stmt_panic<T>(
        &mut self,
        run: impl FnOnce(&mut Self) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
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

    /// v7.38 (read01 P3.26) — transaction-abort firewall around the raw
    /// statement dispatch. After a statement fails inside an explicit
    /// transaction PG aborts the whole block: every later statement except
    /// COMMIT / ROLLBACK / ROLLBACK TO SAVEPOINT is rejected, and a COMMIT
    /// is downgraded to a ROLLBACK so no partial work slips through. We
    /// mirror that here so both the embedded engine and the wire server
    /// enforce it uniformly.
    pub(crate) fn execute_stmt_with_cancel(
        &mut self,
        stmt: Statement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 298) — ask THIS transaction, not "is any
        // transaction anywhere aborted".
        if self.current_tx_aborted() {
            match stmt {
                Statement::Rollback | Statement::RollbackToSavepoint(_) => {}
                // PG performs a ROLLBACK for a COMMIT in an aborted tx.
                Statement::Commit => {
                    let r = self.dispatch_stmt_inner(Statement::Rollback, cancel);
                    self.set_current_tx_aborted(false);
                    return r;
                }
                _ => return Err(EngineError::InFailedTransaction),
            }
        }
        let is_rollback_to_savepoint = matches!(stmt, Statement::RollbackToSavepoint(_));
        // v7.37.17 (Phase E2) — READ COMMITTED per-statement visibility:
        // classify (the statement moves into dispatch below), rebase the
        // open RC tx's shadow onto the latest committed catalog, then
        // record the statement's targets afterwards. Both calls are
        // no-ops outside an explicit transaction.
        let tx_class = crate::classify_stmt_for_tx(&stmt);
        if !matches!(tx_class, crate::TxStmtClass::TxControl) {
            // v7.37.17 (E4 r3) — a unique-key collision found while
            // rebasing fails THIS statement with 40001 (the tx aborts
            // via the standard failed-statement path below, like PG's
            // in-statement 23505 after the lock wait).
            self.maybe_rc_rebase()?;
        }
        // v7.39 (round 552) — what a SERIALIZABLE tx READ, taken before
        // the statement is consumed, recorded after it succeeds.
        let read_tables = crate::transaction::read_tables_of(&stmt);
        let result = self.dispatch_stmt_inner(stmt, cancel);
        if result.is_ok() {
            self.record_tx_stmt(&tx_class);
            self.record_tx_reads(read_tables);
        }
        // v7.39 (round 298) — the witness is THIS connection's slot.
        // `in_transaction()` is true whenever ANY connection holds a
        // transaction, so an autocommit failure used to abort a block
        // that belonged to somebody else.
        let mine_open = self.current_tx.is_some_and(|tx| self.is_tx_open(tx));
        if !mine_open {
            // The tx ended (COMMIT / ROLLBACK) or we were in autocommit;
            // either way there is no aborted block to remember.
            self.set_current_tx_aborted(false);
        } else if result.is_ok() && is_rollback_to_savepoint {
            // Rolling back to a savepoint recovers the transaction.
            self.set_current_tx_aborted(false);
        } else if matches!(result, Err(EngineError::LockWouldBlock)) {
            // v7.39 (round 300) — NOT a failure: the server drops the
            // engine lock and retries. Marking the block aborted here
            // made the FIRST block poison the transaction, so the
            // retry hit the abort firewall and the waiter lost a
            // deadlock it should have won.
        } else if result.is_err() {
            // A failure inside an open transaction aborts the whole block.
            self.set_current_tx_aborted(true);
        }
        result
    }

    pub(crate) fn dispatch_stmt_inner(
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
        // v7.39 (round 305, V23) — evaluate any non-constant LIMIT /
        // OFFSET down to a literal row count. It belongs here, at the
        // one point both the simple-query and the prepared path pass
        // through, because every executor reads the row count as
        // `Option<u32>` and takes `None` for "no limit": an expression
        // that reached execution would silently widen the result to the
        // whole table rather than fail.
        self.resolve_limit_exprs_in_statement(&mut stmt, cancel)?;
        // v7.39 (read01 round 57) — the table-privilege gate. A superuser
        // session (the default login, or `SET ROLE admin`) skips it entirely,
        // so nothing changes for a customer who never assumes another role.
        self.acl_check_statement(&stmt)?;
        // v7.39 (round 435) — MySQL commits an open transaction BEFORE it
        // runs DDL (and before a nested START TRANSACTION), where PG keeps
        // the DDL inside the transaction. A MySQL client that writes rows,
        // runs DDL and then rolls back keeps those rows on MySQL and lost
        // them on SPG — silently, since nothing errors. This is the one
        // point every path (simple query, prepared, extended) passes
        // through, so the commit cannot be skipped by a spelling.
        //
        // v7.39 (round 444) — the witness is THIS connection's slot, not
        // `in_transaction()`. That predicate is true whenever ANY connection
        // holds a transaction, so a second client's `BEGIN` tried to commit a
        // slot of its own that held nothing and answered an error instead —
        // caught by `two_mysql_connections_can_each_hold_a_transaction`, which
        // had been failing since round 435 introduced this hook. Same
        // global-vs-slot confusion rounds 279 / 283 / 298 / 304 each fixed
        // elsewhere; `current_tx` is the connection's own slot here, set by
        // `execute_in_with_cancel` before dispatch.
        let in_own_tx = self.current_tx.is_some_and(|t| self.is_tx_open(t));
        if self.backslash_escapes && in_own_tx && stmt.mysql_implicit_commit() {
            self.exec_commit()?;
        }
        let result = match stmt {
            // v7.39 (round 547) — `ALTER ROLE … SET/RESET` and
            // `ALTER DATABASE … SET/RESET`. These reported success and
            // changed nothing: both fell into the parser's pg_dump
            // no-op tail, so a DBA setting a per-role default got no
            // effect and no error.
            Statement::SetDbRoleSetting(st) => {
                // PG refuses a scope that names something absent.
                // v7.39 (round 696) — one predicate. This wrote its own
                // (`users.any(…) || postgres`), `acl_check_role_exists`
                // wrote a third, and round 652 already recorded what
                // happens when a role predicate and the catalog it reflects
                // disagree. `role_exists` is the one that answers.
                if let Some(role) = &st.role
                    && !self.role_exists(role)
                {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "role \"{role}\" does not exist"
                    )));
                }
                if let Some(db) = &st.database {
                    let current = self
                        .session_params
                        .get("spg.database")
                        .cloned()
                        .unwrap_or_else(|| alloc::string::String::from("spg"));
                    if !db.eq_ignore_ascii_case(&current) {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "database \"{db}\" does not exist"
                        )));
                    }
                }
                let db = st.database.clone().unwrap_or_default();
                let role = st.role.clone().unwrap_or_default();
                let cat = self.active_catalog_mut();
                match (&st.param, &st.value) {
                    (None, _) => cat.reset_db_role_settings(&db, &role),
                    (Some(p), v) => cat.set_db_role_setting(&db, &role, p, v.as_deref()),
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: true,
                })
            }
            // v7.39 (round 430) — MySQL USER variables. The value is an
            // arbitrary expression, evaluated against an empty row (a
            // user-variable assignment is a statement, not a per-row thing),
            // and stored in the session's own namespace.
            //
            // Every right-hand side sees the state as it was BEFORE the
            // statement — the assignments do NOT become visible to each
            // other. Measured on MariaDB 11: with both fresh,
            // `SET @p = 1, @q = @p + 1` leaves @q NULL; with @r already 100,
            // `SET @r = 1, @s = @r + 1` leaves @s at 101, i.e. @r's OLD
            // value. (Separate statements do chain, as you would expect.)
            // So: evaluate them all, THEN apply them all.
            Statement::SetUserVars(assigns, settings) => {
                let mut resolved: Vec<(String, spg_storage::Value<'static>)> =
                    Vec::with_capacity(assigns.len());
                for (name, mut expr) in assigns {
                    // `SET @total = (SELECT SUM(v) FROM t)` is ordinary MySQL,
                    // so the scalar subqueries have to be materialised the way
                    // every other statement's do — eval_expr itself refuses to
                    // meet one.
                    self.resolve_expr_subqueries(&mut expr, cancel)?;
                    let cols: Vec<ColumnSchema> = Vec::new();
                    let value = {
                        let ctx = self.ev_ctx(&cols, None);
                        let empty = spg_storage::Row::new(Vec::new());
                        crate::eval::eval_expr(&expr, &empty, &ctx).map_err(EngineError::Eval)?
                    };
                    resolved.push((name, value.into_owned()));
                }
                for (name, value) in resolved {
                    self.user_vars.insert(name, value);
                }
                // v7.39 (round 554) — the session settings written in
                // the same statement, applied after the saves. Routed
                // through the ordinary SET path so `SQL_MODE` still
                // flips strictness and the rest land where a plain
                // `SET x = y` puts them.
                for (name, value) in settings {
                    let rendered = match crate::conversions::literal_expr_to_value_in(
                        value.clone(),
                        Some(self.active_catalog()),
                    ) {
                        Ok(v) => crate::eval::value_to_text(&v),
                        Err(_) => alloc::format!("{value}"),
                    };
                    let _ = self.execute(&alloc::format!("SET {name} = '{rendered}'"));
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            // v7.39 (round 277) — SQL-level prepared statements.
            Statement::Prepare {
                name,
                param_types,
                body,
                source,
            } => self.exec_prepare(name, param_types, *body, source),
            Statement::Execute { name, args } => self.exec_execute(&name, &args, cancel),
            Statement::Deallocate(name) => self.exec_deallocate(name.as_deref()),
            // v7.39 (round 278) — both were accepted and dropped. They
            // are reported as MISSING OBJECTS rather than as syntax
            // errors, because the SQL parses fine; what is absent is a
            // procedure catalog and a prepared-transaction registry.
            // v7.39 (round 280) — extended statistics as a real
            // catalog object. The planner does not consult them yet;
            // recording them is what makes a pg_dump restore and
            // reflection honest, instead of the statement vanishing.
            Statement::CreateStatistics {
                name,
                if_not_exists,
                kinds,
                columns,
                table,
            } => self.exec_create_statistics(name, if_not_exists, kinds, columns, table),
            Statement::DropStatistics { name, if_exists } => {
                self.exec_drop_statistics(&name, if_exists)
            }
            Statement::Call(name) => Err(EngineError::Unsupported(alloc::format!(
                "procedure {name}() does not exist HINT: No procedure matches the given name \
                 and argument types. You might need to add explicit type casts."
            ))),
            Statement::PrepareTransaction(_) => Err(EngineError::Unsupported(String::from(
                "prepared transactions are disabled HINT: Set \"max_prepared_transactions\" \
                 to a nonzero value.",
            ))),
            Statement::CreateTable(s) => self.exec_create_table(s),
            // v7.39 (round 218) — server-side cursors.
            Statement::DeclareCursor {
                name,
                scroll,
                hold,
                query,
            } => self.exec_declare_cursor(name, scroll, hold, *query),
            Statement::FetchCursor { name, direction } => {
                self.exec_fetch_cursor(&name, direction)
            }
            Statement::MoveCursor { name, direction } => self.exec_move_cursor(&name, direction),
            Statement::CloseCursor { name } => self.exec_close_cursor(name.as_deref()),
            // v7.39 (round 222) — LISTEN/NOTIFY with real delivery.
            Statement::Listen(ch) => self.exec_listen(ch),
            Statement::Notify { channel, payload } => self.exec_notify(channel, payload),
            Statement::Unlisten(ch) => self.exec_unlisten(ch),
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
            // v7.39 (round 695) — `ALTER SYSTEM SET|RESET <name>`. SPG has
            // no postgresql.auto.conf to write, so nothing is APPLIED; what
            // changed is that a name PG18 does not know is now refused
            // instead of accepted. It reuses the session's own GUC check —
            // one place decides what a parameter name means, so `SET` and
            // `ALTER SYSTEM` cannot drift apart in what they accept.
            //
            // The F31 audit found this: the test was called
            // `alter_system_set_no_op` and set `work_mem`, a name that
            // exists, so it could never have caught a name that does not.
            // v7.39 (round 696) — the four statements the F31 sweep found
            // accepting a name that does not exist. SPG still performs
            // nothing for any of them; what changed is that it no longer
            // says "understood" about an object that is not there.
            // v7.39 (round 707) — see Statement::DropAggregate. Existence
            // first across the whole list (PG's order, measured), canonical
            // type names in the signature, and every SPG aggregate is a
            // built-in, so a name that exists is undroppable.
            Statement::DropAggregate { if_exists, items } => {
                let render = |name: &str, args: &Option<Vec<String>>| -> alloc::string::String {
                    match args {
                        None => alloc::format!("{name}(*)"),
                        Some(a) => {
                            let canon: Vec<alloc::string::String> = a
                                .iter()
                                .map(|t| {
                                    crate::conversions::type_name_to_data_type(t).map_or_else(
                                        || t.clone(),
                                        crate::conversions::pg_type_name_for_error,
                                    )
                                })
                                .collect();
                            alloc::format!("{name}({})", canon.join(", "))
                        }
                    }
                };
                for (name, args) in &items {
                    if !crate::aggregate::is_aggregate_name(name.as_str()) {
                        if if_exists {
                            continue;
                        }
                        return Err(EngineError::Unsupported(alloc::format!(
                            "aggregate {} does not exist",
                            render(name, args)
                        )));
                    }
                }
                if let Some((name, args)) = items
                    .iter()
                    .find(|(n, _)| crate::aggregate::is_aggregate_name(n.as_str()))
                {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "cannot drop function {} because it is required by the database system",
                        render(name, args)
                    )));
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            Statement::ValidateOnly { kind, names } => {
                use spg_sql::ast::ValidateOnlyKind as K;
                match kind {
                    K::LockTable => {
                        for n in names {
                            if self.catalog.get(n.as_str()).is_none() {
                                return Err(EngineError::Storage(
                                    spg_storage::StorageError::TableNotFound {
                                        name: n.clone(),
                                    },
                                ));
                            }
                        }
                    }
                    K::RoleName => {
                        for n in names {
                            if !self.role_exists(n.as_str()) {
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "role \"{n}\" does not exist"
                                )));
                            }
                        }
                    }
                    // PG18 refuses this whatever it names, because no label
                    // provider is loaded — and SPG has none either, so the
                    // refusal is the honest answer rather than a stand-in.
                    // v7.39 (round 697) — one list answers both, which is
                    // why these and `pg_extension` cannot disagree.
                    //
                    // A WARNING, not an error, and that is a deliberate
                    // departure from PG. PG can error because an extension
                    // can be installed there; SPG cannot be installed into,
                    // so refusing would turn a customer dump that restores
                    // today into one that needs editing. Saying nothing was
                    // the actual defect: `CREATE EXTENSION hstore` reported
                    // success and nothing hstore-shaped worked afterwards.
                    K::ExtensionAvailable | K::ExtensionInstalled => {
                        for n in names {
                            if !crate::system_catalog::INSTALLED_EXTENSIONS
                                .iter()
                                .any(|(e, _)| e.eq_ignore_ascii_case(n.as_str()))
                            {
                                self.warning(alloc::format!(
                                    "extension \"{n}\" is not provided by this build; SPG \
                                     accepts the statement so a dump restores, but nothing \
                                     that extension supplies will be available"
                                ));
                            }
                        }
                    }
                    // v7.39 (round 708) — ALTER TYPE's no-op forms validate
                    // the name against the three user-type catalogs.
                    K::TypeName => {
                        for n in &names {
                            let cat = self.active_catalog();
                            if !cat.enum_types().contains_key(n)
                                && !cat.domain_types().contains_key(n)
                                && !cat.composite_types().contains_key(n)
                            {
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "type \"{n}\" does not exist"
                                )));
                            }
                        }
                    }
                    // v7.39 (round 708) — names[0] = aggregate, rest = arg
                    // type names; existence by name (round 707's residual on
                    // overloads applies here too).
                    K::AggregateName => {
                        let Some(name) = names.first() else {
                            return Ok(QueryResult::CommandOk {
                                affected: 0,
                                modified_catalog: false,
                            });
                        };
                        if !crate::aggregate::is_aggregate_name(name.as_str()) {
                            let canon: Vec<alloc::string::String> = names[1..]
                                .iter()
                                .map(|t| {
                                    if t == "*" {
                                        alloc::string::String::from("*")
                                    } else {
                                        crate::conversions::type_name_to_data_type(t).map_or_else(
                                            || t.clone(),
                                            crate::conversions::pg_type_name_for_error,
                                        )
                                    }
                                })
                                .collect();
                            return Err(EngineError::Unsupported(alloc::format!(
                                "aggregate {name}({}) does not exist",
                                canon.join(", ")
                            )));
                        }
                    }
                    // v7.39 (round 708) — SPG ships no conversions at all,
                    // so PG's not-found answer is total here.
                    K::ConversionName => {
                        if let Some(n) = names.first() {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "conversion \"{n}\" does not exist"
                            )));
                        }
                    }
                    // v7.39 (round 708) — the shipped languages are
                    // required; anything else does not exist. Both wordings
                    // are PG18 measurements.
                    K::LanguageName => {
                        // One name per statement; PG errors on the first
                        // either way, so `first` says what the loop only
                        // implied (clippy: never actually loops).
                        if let Some(n) = names.first() {
                            let lc = n.to_ascii_lowercase();
                            return Err(EngineError::Unsupported(match lc.as_str() {
                                "plpgsql" => alloc::format!(
                                    "cannot drop language {lc} because extension {lc} requires it"
                                ),
                                "sql" | "internal" | "c" => alloc::format!(
                                    "cannot drop language {lc} because it is required by the database system"
                                ),
                                _ => alloc::format!("language \"{n}\" does not exist"),
                            }));
                        }
                    }
                    // v7.39 (round 706) — see ValidateOnlyKind::ForeignInfra
                    // for why this warns instead of copying PG's refusal.
                    K::ForeignInfra => {
                        self.warning(alloc::string::String::from(
                            "foreign-data infrastructure is not provided by this build; \
                             SPG accepts the statement so a dump restores, but no foreign \
                             server, wrapper or table it defines will function",
                        ));
                    }
                    K::SecurityLabel => {
                        return Err(EngineError::Unsupported(alloc::string::String::from(
                            "no security label providers have been loaded",
                        )));
                    }
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            Statement::AlterSystem { parameter } => {
                if let Some(name) = parameter
                    && let Some(msg) = self.reject_unsettable_guc(name.as_str())
                {
                    return Err(EngineError::Unsupported(msg));
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            Statement::DropTable { names, if_exists } => self.exec_drop_table(names, if_exists),
            Statement::DropIndex { name, if_exists } => self.exec_drop_index(name, if_exists),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => {
                // v7.39 (pg_stat knife A) — per-table n_tup_ins. Charged
                // to the statement's target (a partition-routed insert
                // charges the parent; ON CONFLICT updates count here
                // too — split is a recorded residual).
                let stat_table = s.table.clone();
                let r = self.exec_insert(s)?;
                if let QueryResult::CommandOk { affected, .. } = &r {
                    self.stat_tup_inserted =
                        self.stat_tup_inserted.saturating_add(*affected as u64);
                    // r192 — engine-side, non-transactional (see
                    // table_write_stats): in-tx bumps used to land on
                    // the shadow table and vanish in the RC rebase.
                    self.note_table_write(&stat_table, *affected as u64, 0, 0);
                }
                Ok(r)
            }
            Statement::Update(mut s) => {
                // Materialise uncorrelated subqueries in SET / WHERE
                // before the row walk — the SELECT path has done this
                // since v4.10; UPDATE gained it for mailrs's
                // `UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP
                // LOCKED)` claim pattern (embed round-12).
                // v7.39 (round 157) — NOT with a WITH clause: the CTE
                // temps aren't installed yet here, so a subquery reading
                // a CTE either failed ("relation does not exist") or —
                // when a same-named real table existed — silently read
                // THAT. exec_update_with_ctes resolves after the temps
                // install instead.
                if s.ctes.is_empty() {
                    for (_, e) in &mut s.assignments {
                        self.resolve_expr_subqueries(e, cancel)?;
                    }
                    if let Some(w) = &mut s.where_ {
                        self.resolve_expr_subqueries(w, cancel)?;
                    }
                }
                let r = self.exec_update_cancel(&s, cancel)?;
                if let QueryResult::CommandOk { affected, .. } = &r {
                    self.stat_tup_updated = self.stat_tup_updated.saturating_add(*affected as u64);
                    self.note_table_write(&s.table, 0, *affected as u64, 0);
                }
                Ok(r)
            }
            Statement::Delete(mut s) => {
                // v7.39 (round 157) — see the Update arm: with a WITH
                // clause the resolve runs after the CTE temps install.
                if s.ctes.is_empty()
                    && let Some(w) = &mut s.where_
                {
                    self.resolve_expr_subqueries(w, cancel)?;
                }
                let r = self.exec_delete_cancel(&s, cancel)?;
                if let QueryResult::CommandOk { affected, .. } = &r {
                    self.stat_tup_deleted = self.stat_tup_deleted.saturating_add(*affected as u64);
                    self.note_table_write(&s.table, 0, 0, *affected as u64);
                }
                Ok(r)
            }
            Statement::Merge(s) => self.exec_merge_cancel(&s, cancel),
            // v7.39 (round 295, E3 Phase 1b) — a locking SELECT takes its
            // locks in a `&mut self` pre-pass that respects LIMIT, then
            // runs the ordinary read path with the rows another
            // transaction holds excluded.
            Statement::Select(ref sel) if sel.locking.is_some() => {
                let sel = sel.clone();
                self.lock_skip_rows = None;
                let pre = self.run_locking_prepass(&sel);
                if let Err(e) = pre {
                    self.lock_skip_rows = None;
                    return Err(e);
                }
                let out = self.exec_select_cancel(&sel, cancel);
                self.lock_skip_rows = None;
                out
            }
            Statement::Select(s) => {
                // v7.38 (read01 P3.20) — `SELECT set_config(name, value,
                // is_local)` is the writing sibling of SHOW / current_setting;
                // apply it to the session store (respecting is_local) so the
                // four GUC surfaces stay unified. pg_dump's
                // `SELECT set_config('search_path', '', false)` relies on this.
                if let Some(r) = self.try_exec_set_config(&s)? {
                    return Ok(r);
                }
                if s.ctes.iter().any(|c| c.body.is_modifying()) {
                    self.exec_select_with_modifying_ctes(s, cancel)
                } else {
                    self.exec_select_cancel(&s, cancel)
                }
            }
            // v7.39 (round 249) — the engine is no_std: the HOST reads the
            // file and calls `copy_from_buffer`. Reaching this arm means a
            // host that hasn't wired the file endpoint.
            Statement::CopyFromFile { path, .. } => Err(EngineError::Unsupported(
                alloc::format!(
                    "COPY FROM file: the host must read {path:?} and call copy_from_buffer"
                ),
            )),
            Statement::CopyTo {
                table,
                columns,
                query,
                options,
            } => self.exec_copy_to(
                &table,
                columns.as_deref(),
                query.as_deref(),
                &options,
                cancel,
            ),
            // v7.39 (round 252) — the engine is no_std: the HOST renders
            // via `copy_to_buffer` and writes the file itself.
            Statement::CopyToFile { path, .. } => Err(EngineError::Unsupported(
                alloc::format!(
                    "COPY TO file: the host must render via copy_to_buffer and write {path:?}"
                ),
            )),
            // v7.39 (round 475) — a redundant BEGIN inside a transaction.
            //
            // SPG raised "a transaction is already open" AND left the
            // transaction in the aborted state, so the next statement failed
            // with "current transaction is aborted" and the whole block was
            // lost. A connection pooler or a framework that wraps its own
            // BEGIN around one the caller already opened does this routinely.
            //
            // The two oracles genuinely differ, and both were measured:
            //   PG18       WARNING: there is already a transaction in
            //              progress — the BEGIN is a no-op and the existing
            //              transaction continues (a later ROLLBACK undoes
            //              everything, both rows in the probe).
            //   MariaDB 11 START TRANSACTION implicitly COMMITS the open one
            //              and begins a new one (the first row survives the
            //              rollback, the second does not).
            // The predicate is THIS connection's slot, not the engine-global
            // `in_transaction()`: the server shares one Engine, so the global
            // form makes connection B's BEGIN see connection A's transaction
            // (rounds 279 / 283 / 298 / 304 / 443 / 444 are the same trap).
            Statement::Begin(_)
                if self
                    .current_tx
                    .is_some_and(|t| self.is_tx_open(t))
                    && !self.backslash_escapes =>
            {
                self.warning(alloc::string::String::from(
                    "there is already a transaction in progress",
                ));
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            Statement::Begin(isolation) if self.current_tx.is_some_and(|t| self.is_tx_open(t)) => {
                // MySQL dialect: commit what is open, then start fresh.
                self.exec_commit()?;
                self.exec_begin(isolation)
            }
            Statement::Begin(isolation) => self.exec_begin(isolation),
            // v7.39 (round 435) — a bare COMMIT / ROLLBACK outside a
            // transaction is a no-op that SUCCEEDS. Measured on both
            // oracles: PG18 answers `WARNING: there is no transaction in
            // progress` and still reports COMMIT / ROLLBACK; MariaDB 11
            // succeeds silently. SPG answered "no active transaction" as an
            // ERROR to both dialects — a divergence from each of them.
            // It moved onto the hot path with the implicit-commit rule
            // above, which leaves a client's trailing ROLLBACK with nothing
            // to roll back.
            Statement::Commit | Statement::Rollback if !self.in_transaction() => {
                if !self.backslash_escapes {
                    self.warning(alloc::string::String::from(
                        "there is no transaction in progress",
                    ));
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
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
            Statement::Kill { query_only, id } => self.exec_kill(query_only, &id),
            Statement::Discard(target) => self.exec_discard(target),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            Statement::ShowPublications => Ok(self.exec_show_publications()),
            Statement::ShowSubscriptions => Ok(self.exec_show_subscriptions()),
            Statement::CreateUser(s) => self.exec_create_user(&s),
            Statement::DropUser { name, if_exists } => self.exec_drop_user(&name, if_exists),
            Statement::SetRole(role) => {
                match role {
                    Some(name) => {
                        // v7.39 (read01 round 58) — PG rejects a SET ROLE to a
                        // role that does not exist. Before roles were real
                        // there was nothing to check against, so any name was
                        // accepted — and a typo silently put the session into
                        // a role that held nothing.
                        self.acl_check_role_exists(&name)?;
                        self.session_params.insert(
                            alloc::string::String::from(crate::session::CURRENT_ROLE_KEY),
                            name,
                        );
                    }
                    None => {
                        self.session_params.remove(crate::session::CURRENT_ROLE_KEY);
                    }
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            Statement::Grant(g) => self.exec_grant(&g, true),
            Statement::Revoke(g) => self.exec_grant(&g, false),
            Statement::CreatePolicy(s) => self.exec_create_policy(s),
            Statement::AlterPolicy(s) => self.exec_alter_policy(s),
            Statement::DropPolicy(s) => self.exec_drop_policy(s),
            // v7.39 (round 286) — ANALYZE over DML really executes, so it
            // needs the `&mut self` sibling. Everything else (including
            // plain EXPLAIN of a write) stays on the read-only renderer.
            // v7.39 (round 288) — SET CONSTRAINTS sets the timing for the
            // rest of the transaction. IMMEDIATE also runs everything the
            // transaction has postponed, right here — PG raises the
            // violation at this statement, not at COMMIT.
            Statement::SetConstraints { names, deferred } => {
                self.exec_set_constraints(&names, deferred)
            }
            Statement::Explain(e)
                if e.analyze
                    && !e.suggest
                    && matches!(
                        &*e.inner,
                        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
                    ) =>
            {
                self.exec_explain_analyze_dml(&e, cancel)
            }
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
            // v7.39 (round 535) — REINDEX / CLUSTER. SPG has neither index
            // bloat to rebuild nor a clustering order to impose, so the
            // work is a no-op — but PG VALIDATES the target, and both
            // statements were swallowed at parse time AND intercepted at
            // the wire, so `REINDEX TABLE typo` answered `REINDEX`. A
            // maintenance script that misspells a table was told it
            // succeeded.
            Statement::Maintain { kind, target } => {
                use spg_sql::ast::MaintainKind;
                match (kind, target.as_deref()) {
                    (MaintainKind::ReindexRelation | MaintainKind::ClusterRelation, Some(t)) => {
                        // An INDEX is a relation too — `REINDEX INDEX ix`
                        // names one, and looking only at tables refused a
                        // name that is right there.
                        let is_index = self
                            .active_catalog()
                            .table_names()
                            .iter()
                            .filter_map(|n| self.active_catalog().get(n))
                            .any(|tbl| {
                                tbl.indices().iter().any(|i| i.name.eq_ignore_ascii_case(t))
                            });
                        if !is_index && self.active_catalog().get(t).is_none() {
                            return Err(EngineError::Storage(
                                spg_storage::StorageError::TableNotFound { name: t.into() },
                            ));
                        }
                    }
                    (MaintainKind::ReindexSchema, Some(t)) => {
                        if !spg_storage::is_builtin_schema(t)
                            && !self.active_catalog().schema_exists(t)
                        {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "schema \"{t}\" does not exist"
                            )));
                        }
                    }
                    // `REINDEX SYSTEM` / `REINDEX DATABASE` / a bare
                    // `CLUSTER` name nothing to check.
                    _ => {}
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
            // v7.39 (round 169) — VACUUM does real work under the MVCC
            // gate (tombstoned versions are actual bloat); the pre-MVCC
            // parse-time no-op silently ignored a customer's manual
            // reclaim. Gate-off stays a provable no-op inside vacuum.
            Statement::Vacuum { table, analyze } => {
                match &table {
                    Some(t) => {
                        // v7.39 (round 535) — PG refuses a VACUUM whose
                        // relation does not exist; `vacuum_one_table`
                        // simply found nothing to do and said nothing,
                        // so a typo'd table reported success.
                        if self.active_catalog().get(t).is_none() {
                            return Err(EngineError::Storage(
                                spg_storage::StorageError::TableNotFound { name: t.clone() },
                            ));
                        }
                        self.vacuum_one_table(t);
                    }
                    None => {
                        let _ = self.vacuum_pass(false);
                    }
                }
                if analyze {
                    self.exec_analyze(table.as_deref())?;
                }
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                })
            }
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
                only,
            } => self.exec_truncate(tables.as_slice(), restart_identity, only),
            // v6.7.3 — COMPACT COLD SEGMENTS.
            Statement::CompactColdSegments => self.exec_compact_cold_segments(),
            // v7.12.1 — SET / RESET session parameter. Engine
            // tracks the value in `session_params`; FTS dispatcher
            // reads `default_text_search_config`. Everything else
            // is a recorded no-op (PG dump compat).
            Statement::SetParameter { name, value, local } => {
                // v7.39 (round 501) — a name PG18 does not know, or one a
                // session cannot change, is an error there and was
                // silently accepted here (round 500).
                if let Some(msg) = self.reject_unsettable_guc(&name) {
                    return Err(EngineError::Unsupported(msg));
                }
                // v7.38 (read01) — SPG serves the wire as UTF8, so a
                // non-UTF8 client_encoding can't be honoured (the bytes
                // stay UTF8). Reject it rather than silently store a value
                // that would mislabel the stream; an unusable name is
                // rejected the way PG rejects an invalid one.
                if name.eq_ignore_ascii_case("client_encoding") {
                    let v: &str = match &value {
                        spg_sql::ast::SetValue::String(s)
                        | spg_sql::ast::SetValue::Ident(s)
                        | spg_sql::ast::SetValue::Number(s) => s.as_str(),
                        spg_sql::ast::SetValue::Default => "UTF8",
                    };
                    let norm: alloc::string::String = v
                        .trim()
                        .to_ascii_uppercase()
                        .chars()
                        .filter(|c| *c != '-' && *c != '_')
                        .collect();
                    if !matches!(norm.as_str(), "UTF8" | "UNICODE") {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "invalid value for parameter \"client_encoding\": \"{v}\" \
                             (SPG serves UTF8 only)"
                        )));
                    }
                }
                // v7.38 (read01 P3.17) — reject a clearly-invalid value for
                // a handful of well-known typed GUCs (`SET work_mem =
                // 'bogus'` errors like PG). Unknown GUCs stay accept-and-
                // record for pg_dump compat.
                if let spg_sql::ast::SetValue::String(s)
                | spg_sql::ast::SetValue::Ident(s)
                | spg_sql::ast::SetValue::Number(s) = &value
                {
                    validate_known_guc(&name, s)?;
                    // v7.39 (tz epic) — timezone accepts UTC / fixed
                    // offsets / abbreviations (resolve_zone_offset) and
                    // IANA names (host tzdb); anything else is PG's
                    // invalid-parameter error. Named zones store their
                    // canonical spelling (SHOW returns 'Asia/Tokyo'
                    // after SET 'asia/tokyo').
                    if name.eq_ignore_ascii_case("timezone")
                        || name.eq_ignore_ascii_case("time zone")
                    {
                        let canon = self.canonicalize_timezone(s)?;
                        let local = local;
                        if local {
                            if self.in_transaction() {
                                let prior = self.session_param("timezone").map(String::from);
                                self.local_guc_saves.push(("timezone".into(), prior));
                                self.set_session_param(
                                    "timezone".into(),
                                    spg_sql::ast::SetValue::String(canon),
                                );
                            }
                        } else {
                            self.set_session_param(
                                "timezone".into(),
                                spg_sql::ast::SetValue::String(canon),
                            );
                        }
                        return Ok(QueryResult::CommandOk {
                            affected: 0,
                            modified_catalog: false,
                        });
                    }
                }
                // v7.38 (read01 P3.19) — `SET LOCAL` scopes the change to
                // the current transaction: record the prior value in the
                // undo log so COMMIT / ROLLBACK (and ROLLBACK TO) restore
                // it. Outside a transaction block it has no lasting effect
                // (PG scopes it to the implicit single-statement txn), so
                // it is dropped rather than persisted to the session.
                if local {
                    if self.in_transaction() {
                        let prior = self.session_param(&name).map(String::from);
                        self.local_guc_saves.push((name.clone(), prior));
                        self.set_session_param(name, value);
                    }
                } else {
                    self.set_session_param(name, value);
                }
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
                // v7.37.17 (Phase E3) — PG rejects an isolation switch
                // after the transaction's first query (SQLSTATE 25001);
                // silently applying it to the remaining statements would
                // give a tx that is half one level, half another.
                if let Some(tx_id) = self.current_tx
                    && self
                        .tx_catalogs
                        .get(&tx_id)
                        .is_some_and(|st| st.stmts_run > 0)
                {
                    return Err(EngineError::Unsupported(
                        "SET TRANSACTION ISOLATION LEVEL must be called before any query".into(),
                    ));
                }
                self.current_isolation_level = isolation;
                // v7.37.17 (Phase E2) — inside an open tx, switching to
                // RR/SER BEFORE the first query freezes the tx's view by
                // caching a snapshot now (PG allows the switch until the
                // first query; the RC rebase keys off cached_snapshot).
                // Switching (back) to RC/RU clears it so the rebase
                // resumes.
                if let Some(tx_id) = self.current_tx
                    && self.tx_catalogs.contains_key(&tx_id)
                {
                    let cache = match isolation {
                        spg_sql::ast::IsolationLevel::RepeatableRead
                        | spg_sql::ast::IsolationLevel::Serializable => {
                            Some(self.current_snapshot())
                        }
                        spg_sql::ast::IsolationLevel::ReadUncommitted
                        | spg_sql::ast::IsolationLevel::ReadCommitted => None,
                    };
                    if let Some(st) = self.tx_catalogs.get_mut(&tx_id) {
                        st.cached_snapshot = cache;
                    }
                }
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
                // v7.38 (read01 P3.20/P3.23) — SHOW reads the same canonical
                // GUC inventory as pg_settings, so `SHOW <name>` / `SHOW ALL`
                // and pg_settings never disagree on which params exist.
                let canon = crate::system_catalog::canonical_gucs();
                let effective = |n: &str, boot: &str| -> alloc::string::String {
                    self.session_params
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(n))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| boot.into())
                };
                if name.eq_ignore_ascii_case("all") {
                    let cols = alloc::vec![
                        ColumnSchema::new("name", DataType::Text, false),
                        ColumnSchema::new("setting", DataType::Text, false),
                        ColumnSchema::new("description", DataType::Text, false),
                    ];
                    let mut rows: Vec<Row> = Vec::new();
                    // Dynamic params outside the static canonical table.
                    rows.push(Row::new(alloc::vec![
                        Value::text(alloc::string::String::from("transaction_isolation")),
                        Value::text(alloc::string::String::from(
                            self.current_isolation_level.as_pg_str(),
                        )),
                        Value::text(alloc::string::String::from(
                            "Shows the current transaction's isolation level.",
                        )),
                    ]));
                    rows.push(Row::new(alloc::vec![
                        Value::text(alloc::string::String::from("is_superuser")),
                        Value::text(alloc::string::String::from("on")),
                        Value::text(alloc::string::String::from("Reports superuser status.")),
                    ]));
                    for (n, boot, cat, _, _) in canon {
                        rows.push(Row::new(alloc::vec![
                            Value::text(alloc::string::String::from(*n)),
                            Value::text(effective(n, boot)),
                            Value::text(alloc::string::String::from(*cat)),
                        ]));
                    }
                    return Ok(QueryResult::Rows {
                        columns: cols,
                        rows,
                    });
                }
                let value: alloc::string::String = match name.to_ascii_lowercase().as_str() {
                    "transaction_isolation" => {
                        alloc::string::String::from(self.current_isolation_level.as_pg_str())
                    }
                    "is_superuser" => alloc::string::String::from("on"),
                    _ => {
                        // Canonical GUC? report the session override or its
                        // boot default. Otherwise a user-set custom GUC, or a
                        // recognised-name error pointing at pg_settings.
                        if let Some((_, boot, ..)) =
                            canon.iter().find(|(n, ..)| n.eq_ignore_ascii_case(&name))
                        {
                            effective(&name, boot)
                        } else if let Some(v) = self.session_param(&name) {
                            alloc::string::String::from(v)
                        } else if let Some(boot) =
                            crate::guc_catalog::guc_boot_value(&name)
                        {
                            // v7.39 (round 534) — a parameter PG18 knows but
                            // SPG does not model reports its compiled-in
                            // default. `SHOW random_page_cost` printed
                            // nothing at all before, and `SHOW fsync` with
                            // it.
                            alloc::string::String::from(boot)
                        } else {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "SHOW {name:?}: parameter not recognised; \
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
                // Same validation as the single form (round 501).
                for (name, _) in &pairs {
                    if let Some(msg) = self.reject_unsettable_guc(name) {
                        return Err(EngineError::Unsupported(msg));
                    }
                }
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
            Statement::CreateRule(s) => self.exec_create_rule(s),
            Statement::DropRule {
                name,
                table,
                if_exists,
            } => self.exec_drop_rule(&name, &table, if_exists),
            Statement::DropFunction {
                name,
                args,
                if_exists,
            } => self.exec_drop_function(&name, args.as_deref(), if_exists),
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
            Statement::CommentOn {
                kind,
                name,
                comment,
            } => self.exec_comment_on(&kind, &name, comment.as_deref()),
            Statement::AlterTypeRenameValue {
                type_name,
                old,
                new,
            } => {
                self.active_catalog_mut()
                    .rename_enum_value(&type_name, &old, &new)
                    .map_err(EngineError::Storage)?;
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: !self.in_transaction(),
                })
            }
            Statement::AlterTypeAddValue {
                type_name,
                label,
                if_not_exists,
                position,
            } => {
                let added = self
                    .active_catalog_mut()
                    .add_enum_value(&type_name, &label, if_not_exists, position)
                    .map_err(EngineError::Storage)?;
                Ok(QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: added,
                })
            }
            Statement::DropType { names, if_exists } => self.exec_drop_type(&names, if_exists),
            Statement::CreateDomain(s) => self.exec_create_domain(s),
            Statement::AlterDomain { name, action } => self.exec_alter_domain(&name, action),
            Statement::DropDomain { names, if_exists } => self.exec_drop_domain(&names, if_exists),
            Statement::CreateSchema {
                name,
                if_not_exists,
            } => self.exec_create_schema(name, if_not_exists),
            Statement::DropSchema { names, if_exists } => self.exec_drop_schema(&names, if_exists),
            Statement::ResetParameter(target) => {
                match target {
                    // v7.39 (round 320, V53) — RESET ALL resets GUCs. It
                    // must NOT throw away the two internal keys the server
                    // parks in the same map: the connection's login
                    // identity and its database. PG has no way to reset
                    // those with RESET ALL (they are not GUCs), and
                    // clearing them here made `current_user` fall back to
                    // the admin default mid-session.
                    None => self.reset_all_gucs(),
                    Some(name) => {
                        self.session_params.remove(&name.to_ascii_lowercase());
                    }
                }
                self.refresh_render_style();
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
    /// v7.39 (round 247) — resolve the CSV-only extras. QUOTE / ESCAPE /
    /// FORCE_QUOTE outside CSV mode are PG's 0A000 refusals (SPG used to
    /// ignore a text-mode QUOTE silently); the returned mask marks the
    /// force-quoted columns of `column_names`.
    fn resolve_copy_csv_extras(
        options: &spg_sql::ast::CopyOptions,
        is_csv: bool,
        quote: char,
        column_names: &[alloc::string::String],
    ) -> Result<(char, Option<alloc::vec::Vec<bool>>), EngineError> {
        if !is_csv {
            if options.quote.is_some() {
                return Err(EngineError::Unsupported("COPY QUOTE requires CSV mode".into()));
            }
            if options.escape.is_some() {
                return Err(EngineError::Unsupported("COPY ESCAPE requires CSV mode".into()));
            }
        }
        // v7.39 (round 265) — the direction-dependent rules (FORCE_QUOTE is
        // TO-only, FORCE_NOT_NULL / FORCE_NULL are FROM-only), sharing one
        // validator with the FROM path.
        crate::copy::validate_copy_option_direction(options, true)?;
        let escape = options.escape.unwrap_or(quote);
        let force = match &options.force_quote {
            None => None,
            Some(cols) if cols.is_empty() => Some(alloc::vec![true; column_names.len()]),
            Some(cols) => {
                let mut mask = alloc::vec![false; column_names.len()];
                for c in cols {
                    let pos = column_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(c))
                        .ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "column \"{c}\" does not exist"
                            ))
                        })?;
                    mask[pos] = true;
                }
                Some(mask)
            }
        };
        Ok((escape, force))
    }


    /// v7.39 (round 249) — resolve the effective COPY FROM target column
    /// list, running PG's pre-file checks in PG's order: the relation
    /// must exist, an explicit column must exist on it, and no column
    /// may appear twice — all before a single data row is looked at.
    ///
    /// # Errors
    /// `relation "t" does not exist`, `column "x" of relation "t" does
    /// not exist` (42703), `column "x" specified more than once` (42701).
    /// v7.39 (round 343, V40) — store a file the host just read as a
    /// large object. The host does the IO (the engine is `no_std`); the
    /// catalog side is the same `create_large_object` the rest of the
    /// lo_* family uses, so an imported object is indistinguishable from
    /// one built with `lo_from_bytea`.
    pub fn lo_import_bytes(
        &mut self,
        want_oid: u32,
        data: alloc::vec::Vec<u8>,
    ) -> Result<u32, EngineError> {
        self.active_catalog_mut()
            .create_large_object(want_oid, data)
            .map_err(EngineError::Unsupported)
    }

    /// v7.39 (round 343, V40) — the bytes the host is about to write out.
    /// PG's message for a missing object, verbatim.
    pub fn lo_export_bytes(&self, oid: u32) -> Result<alloc::vec::Vec<u8>, EngineError> {
        self.active_catalog()
            .large_object(oid)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("large object {oid} does not exist"))
            })
    }

    pub fn copy_target_columns(
        &self,
        table: &str,
        columns: Option<&[alloc::string::String]>,
    ) -> Result<alloc::vec::Vec<alloc::string::String>, EngineError> {
        let table_ref = self.active_catalog().get(table).ok_or_else(|| {
            EngineError::Storage(spg_storage::StorageError::TableNotFound {
                name: alloc::string::String::from(table),
            })
        })?;
        let schema_cols = &table_ref.schema().columns;
        match columns {
            None => Ok(schema_cols.iter().map(|c| c.name.clone()).collect()),
            Some(cols) => {
                for (i, name) in cols.iter().enumerate() {
                    if !schema_cols.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{name}\" of relation \"{table}\" does not exist"
                        )));
                    }
                    if cols[..i].iter().any(|p| p.eq_ignore_ascii_case(name)) {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{name}\" specified more than once"
                        )));
                    }
                }
                Ok(cols.to_vec())
            }
        }
    }

    /// v7.39 (round 249) — execute a parsed `COPY … FROM '<file>'` whose
    /// file contents the HOST has already read (the engine is no_std and
    /// performs no I/O). Lowers to per-row INSERTs via
    /// [`crate::copy::copy_buffer_inserts`]; outside an explicit
    /// transaction the rows are wrapped in one, so a bad row aborts the
    /// whole COPY exactly as in PG.
    ///
    /// # Errors
    /// The failing row's INSERT error propagates (after rollback).
    pub fn copy_from_buffer(
        &mut self,
        table: &str,
        columns: Option<&[alloc::string::String]>,
        options: &spg_sql::ast::CopyOptions,
        data: &str,
    ) -> Result<QueryResult, EngineError> {
        let target = self.copy_target_columns(table, columns)?;
        let inserts =
            crate::copy::copy_buffer_inserts(table, columns, &target, options, data)?;
        let wrap = !self.in_transaction();
        if wrap {
            self.execute("BEGIN")?;
        }
        let mut affected: usize = 0;
        for insert in &inserts {
            match self.execute(insert) {
                Ok(QueryResult::CommandOk { affected: n, .. }) => affected += n,
                Ok(_) => affected += 1,
                Err(e) => {
                    if wrap {
                        let _ = self.execute("ROLLBACK");
                    }
                    return Err(e);
                }
            }
        }
        if wrap {
            self.execute("COMMIT")?;
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: false,
        })
    }

    /// v7.39 (round 252) — render a `COPY … TO '<file>'` payload for the
    /// HOST to write (the engine is no_std and performs no I/O). Returns
    /// the encoded bytes (one line per record, trailing newline) and the
    /// DATA row count for the `COPY n` tag — the HEADER line, when
    /// present, is part of the payload but not of the count.
    ///
    /// # Errors
    /// Same surface as `COPY … TO STDOUT` (missing relation / column,
    /// CSV-mode option refusals).
    pub fn copy_to_buffer(
        &mut self,
        table: &str,
        columns: Option<&[alloc::string::String]>,
        query: Option<&Statement>,
        options: &spg_sql::ast::CopyOptions,
    ) -> Result<(alloc::string::String, usize), EngineError> {
        let result = self.exec_copy_to(table, columns, query, options, CancelToken::none())?;
        let QueryResult::Rows { rows, .. } = result else {
            return Err(EngineError::Unsupported(
                "COPY TO rendered a non-row result".into(),
            ));
        };
        let mut payload = alloc::string::String::new();
        for row in &rows {
            if let Some(Value::Text(line)) = row.values.first() {
                payload.push_str(line);
            }
            payload.push('\n');
        }
        let data_rows = rows.len().saturating_sub(usize::from(options.header));
        Ok((payload, data_rows))
    }

    /// `COPY table [(cols)] TO STDOUT` — render the visible rows
    /// in COPY text format (tab-separated, `\N` nulls, backslash
    /// escapes) as a single-text-column result set. Embedded
    /// consumers read the lines directly; the wire layer streams
    /// CopyData frames from them.
    fn exec_copy_to(
        &mut self,
        table_name: &str,
        columns: Option<&[String]>,
        query: Option<&Statement>,
        options: &spg_sql::ast::CopyOptions,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        use spg_sql::ast::CopyFormat;
        // v7.39 (read01 round 94) — `COPY (<query>) TO STDOUT`: run the inner
        // statement and render its result set with the same per-format cell
        // encoder the table form uses. Kept as an early branch so the
        // battle-tested table path below is untouched.
        if let Some(q) = query {
            return self.exec_copy_to_query(q, options, cancel);
        }
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
        // Per-format defaults: text = tab / `\N`; csv = comma / `` / `"`.
        let is_csv = options.format == CopyFormat::Csv;
        let delimiter = options.delimiter.unwrap_or(if is_csv { ',' } else { '\t' });
        let quote = options.quote.unwrap_or('"');
        let null_str = options
            .null_str
            .clone()
            .unwrap_or_else(|| alloc::string::String::from(if is_csv { "" } else { "\\N" }));
        // v7.39 (round 247) — the FORCE_QUOTE mask follows the emitted
        // column order (the projection), not the table order.
        let out_names: alloc::vec::Vec<alloc::string::String> = positions
            .iter()
            .filter_map(|&p| schema_cols.get(p).map(|c| c.name.clone()))
            .collect();
        let (escape, force_mask) = Self::resolve_copy_csv_extras(options, is_csv, quote, &out_names)?;
        let encode_cells = |cells: &[Option<alloc::string::String>]| -> alloc::string::String {
            if is_csv {
                crate::copy::encode_copy_csv_cells_opts(
                    cells,
                    delimiter,
                    quote,
                    escape,
                    force_mask.as_deref(),
                    &null_str,
                )
            } else {
                crate::copy::encode_copy_text_cells_opts(cells, delimiter, &null_str)
            }
        };
        let snap = self.current_snapshot();
        let mut out_rows: alloc::vec::Vec<spg_storage::Row<'static>> = alloc::vec::Vec::new();
        // HEADER: the selected column names as the first line, encoded
        // per the same format rules (a name is never NULL).
        if options.header {
            let names: alloc::vec::Vec<Option<alloc::string::String>> = positions
                .iter()
                .map(|&p| Some(schema_cols[p].name.clone()))
                .collect();
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(
                encode_cells(&names)
            )]));
        }
        // COPY renders each value with its type's output function, the
        // same as the wire — notably bool as `t` / `f`, not the engine's
        // debug-ish `true` / `false`.
        // v7.38 (T-tstz Phase 1) — `ty` is the column's declared type, needed
        // only to tell timestamptz from timestamp: PG's COPY renders the former
        // with its offset. Everything else renders identically either way.
        let cell_text = |v: &Value, ty: spg_storage::DataType| -> Option<alloc::string::String> {
            match v {
                Value::Null => None,
                Value::Bool(b) => Some(alloc::string::String::from(if *b { "t" } else { "f" })),
                Value::Timestamp(t) if matches!(ty, spg_storage::DataType::Timestamptz) => {
                    Some(crate::eval::format_timestamptz(*t))
                }
                other => Some(crate::eval::values::value_to_text(other)),
            }
        };
        let encode = |row: &spg_storage::Row<'static>| {
            let cells: alloc::vec::Vec<Option<alloc::string::String>> = positions
                .iter()
                .map(|&p| {
                    row.values
                        .get(p)
                        .and_then(|v| cell_text(v, schema_cols[p].ty))
                })
                .collect();
            encode_cells(&cells)
        };
        for (_, row) in table.scan_visible(&snap) {
            cancel.check()?;
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(encode(row))]));
        }
        for row in self.iter_cold_rows_of_table(table) {
            cancel.check()?;
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(encode(
                &row
            ))]));
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

    /// v7.39 (read01 round 94) — the `COPY (<query>) TO STDOUT` renderer.
    /// Executes the inner statement and encodes its result set into a single
    /// `copy` text column (one row per COPY line, header first when asked),
    /// exactly like the table form's tail — the difference is only where the
    /// rows and their column types come from.
    fn exec_copy_to_query(
        &mut self,
        query: &Statement,
        options: &spg_sql::ast::CopyOptions,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        use spg_sql::ast::CopyFormat;
        let (result_cols, result_rows) = match self.dispatch_stmt_inner(query.clone(), cancel)? {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => {
                return Err(EngineError::Unsupported(
                    "COPY (query) source did not produce a result set".into(),
                ));
            }
        };
        let is_csv = options.format == CopyFormat::Csv;
        let delimiter = options.delimiter.unwrap_or(if is_csv { ',' } else { '\t' });
        let quote = options.quote.unwrap_or('"');
        let null_str = options
            .null_str
            .clone()
            .unwrap_or_else(|| alloc::string::String::from(if is_csv { "" } else { "\\N" }));
        let out_names: alloc::vec::Vec<alloc::string::String> =
            result_cols.iter().map(|c| c.name.clone()).collect();
        let (escape, force_mask) = Self::resolve_copy_csv_extras(options, is_csv, quote, &out_names)?;
        let encode_cells = |cells: &[Option<alloc::string::String>]| -> alloc::string::String {
            if is_csv {
                crate::copy::encode_copy_csv_cells_opts(
                    cells,
                    delimiter,
                    quote,
                    escape,
                    force_mask.as_deref(),
                    &null_str,
                )
            } else {
                crate::copy::encode_copy_text_cells_opts(cells, delimiter, &null_str)
            }
        };
        let cell_text = |v: &Value, ty: spg_storage::DataType| -> Option<alloc::string::String> {
            match v {
                Value::Null => None,
                Value::Bool(b) => Some(alloc::string::String::from(if *b { "t" } else { "f" })),
                Value::Timestamp(t) if matches!(ty, spg_storage::DataType::Timestamptz) => {
                    Some(crate::eval::format_timestamptz(*t))
                }
                other => Some(crate::eval::values::value_to_text(other)),
            }
        };
        let mut out_rows: alloc::vec::Vec<spg_storage::Row<'static>> = alloc::vec::Vec::new();
        if options.header {
            let names: alloc::vec::Vec<Option<alloc::string::String>> =
                result_cols.iter().map(|c| Some(c.name.clone())).collect();
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(
                encode_cells(&names)
            )]));
        }
        for row in &result_rows {
            cancel.check()?;
            let cells: alloc::vec::Vec<Option<alloc::string::String>> = result_cols
                .iter()
                .enumerate()
                .map(|(p, c)| row.values.get(p).and_then(|v| cell_text(v, c.ty)))
                .collect();
            out_rows.push(spg_storage::Row::new(alloc::vec![Value::text(
                encode_cells(&cells)
            )]));
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
