//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
mod bytebudget;
mod clock;
mod constraints;
mod conversions;
pub mod copy;
mod ddl;
pub mod describe;
mod dml;
mod envelope;
pub mod eval;
mod explain;
mod expr_analysis;
pub mod fts;
mod index_access;
mod join;
pub mod json;
mod maintenance;
pub mod memoize;
mod numeric;
mod orderby;
pub mod plan_cache;
mod plpgsql;
pub mod publications;
pub mod query_stats;
pub mod reorder;
mod select;
pub mod selectivity;
mod sequence;
mod session;
mod show;
mod spg_admin;
pub mod statistics;
mod subquery;
pub mod subscriptions;
mod substitute;
mod system_catalog;
mod table_access;
mod transaction;
pub mod triggers;
pub mod users;
mod window;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};

use bytebudget::*;
pub(crate) use clock::{rewrite_clock_calls, value_to_literal};
use constraints::*;
use conversions::*;
pub use conversions::{
    format_bigint_2d_text_pub, format_hstore_text, format_int_2d_text_pub, format_range_text,
    format_text_2d_text_pub,
};
pub(crate) use ddl::{
    canonicalize_set_value, enforce_enum_label, eval_runtime_default_free,
    resolve_column_default_free,
};
pub(crate) use envelope::{EnvelopeParse, build_envelope, split_envelope};
use expr_analysis::*;
use index_access::*;
pub(crate) use orderby::{
    apply_offset_and_limit, apply_offset_and_limit_tagged, build_order_keys, canonical_value_repr,
    expand_group_by_all, order_by_value_cmp, partial_sort_tagged, render_histogram_bounds,
    resolve_order_by_position, sort_by_keys, sort_values_for_histogram, value_cmp, value_to_f64,
};
pub(crate) use select::{build_projection, infer_column_types, value_to_order_key};
pub(crate) use show::render_create_table;
pub(crate) use subquery::{
    build_in_list_set, collect_scalar_subqueries, expr_has_subquery, expr_tree_has_subquery,
};
pub use substitute::substitute_placeholders;
use substitute::*;
use system_catalog::*;
use window::*;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::Statement;
// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::Statement as ParsedStatement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{Catalog, ColumnSchema, Row, StorageError, Value};

use crate::eval::EvalError;

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum QueryResult {
    /// DDL or DML succeeded.
    ///
    /// `affected` is the row count for `INSERT` and 0 elsewhere.
    /// `modified_catalog` tells the server whether this statement
    /// caused the *committed* catalog to change — it's the signal to
    /// snapshot/audit. False for `BEGIN`/`ROLLBACK`, false for writeful
    /// statements executed inside a transaction (those only touch the
    /// shadow), and true for `COMMIT` and for writes outside a TX.
    CommandOk {
        affected: usize,
        modified_catalog: bool,
    },
    /// `SELECT` returned a (possibly empty) row set.
    Rows {
        columns: Vec<ColumnSchema>,
        rows: Vec<Row>,
    },
}

/// All errors the engine can return.
///
/// Marked `#[non_exhaustive]` from v7.5.0 onward: external `match`
/// must include a `_` arm so new variants in subsequent v7.x releases
/// are not breaking changes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineError {
    Parse(ParseError),
    Storage(StorageError),
    Eval(EvalError),
    /// Front-end accepted a construct that the v0.x executor doesn't support.
    Unsupported(String),
    /// `BEGIN` while another transaction is already open.
    TransactionAlreadyOpen,
    /// `COMMIT` / `ROLLBACK` with no active transaction.
    NoActiveTransaction,
    /// v4.0 sentinel: `execute_readonly` got a statement that
    /// mutates engine state (INSERT / CREATE / BEGIN / COMMIT / …).
    /// The caller should retake the write lock and dispatch through
    /// `execute(&mut self)` instead.
    WriteRequired,
    /// v4.2: a SELECT would have returned more rows than the
    /// configured `max_query_rows` cap. Carries the cap.
    RowLimitExceeded(usize),
    /// v7.30.3 (mailrs round-26): a SELECT's join/filter
    /// materialisation would have held more (approximate) heap
    /// bytes than the configured `max_query_bytes` cap. The row
    /// cap above counts rows; this counts bytes, because one row
    /// can be a multi-MB mail body — 1000 fat rows pressure the
    /// host long before any row ceiling trips. Carries the cap.
    QueryBytesExceeded(usize),
    /// v4.5: cooperative cancellation — the host (server's
    /// per-query watchdog) set the cancel flag while a long-running
    /// SELECT / UPDATE / DELETE was scanning rows. The partial work
    /// is discarded; the caller should surface this as a timeout
    /// to the client.
    Cancelled,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::Eval(e) => write!(f, "eval: {e}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::TransactionAlreadyOpen => f.write_str("a transaction is already open"),
            Self::NoActiveTransaction => f.write_str("no active transaction"),
            Self::WriteRequired => {
                f.write_str("statement requires a write lock (use execute, not execute_readonly)")
            }
            Self::RowLimitExceeded(n) => {
                write!(f, "query exceeded max_query_rows={n}")
            }
            Self::QueryBytesExceeded(n) => {
                write!(
                    f,
                    "query materialisation exceeded max_query_bytes={n} (set SPG_MAX_QUERY_BYTES to raise, 0 to disable)"
                )
            }
            Self::Cancelled => f.write_str("query cancelled (timeout or client request)"),
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}
impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<EvalError> for EngineError {
    fn from(e: EvalError) -> Self {
        Self::Eval(e)
    }
}

/// The execution engine. Holds the catalog and (later) other server-scope
/// state. `Engine::new()` is intentionally cheap so callers can construct one
/// per database, per test.
/// Function pointer that returns "now" as microseconds since Unix
/// epoch. The engine is `no_std`, so it can't reach for `std::time`
/// itself — callers (`spg-server`, the sqllogictest runner) inject a
/// concrete implementation. `None` means `NOW()` / `CURRENT_*` raise
/// `Unsupported`.
pub type ClockFn = fn() -> i64;

/// Function pointer that produces 16 cryptographically random bytes.
/// Like `ClockFn`, the engine is `no_std` and can't reach for /dev/urandom
/// itself — host (`spg-server`) injects an OS-backed source. `None`
/// means SQL-driven `CREATE USER` falls back to a deterministic salt
/// derived from the username (acceptable in tests; the server always
/// installs a real RNG so production paths never see this).
pub type SaltFn = fn() -> [u8; 16];

/// v4.5 cooperative cancellation token. A long-running SELECT /
/// UPDATE / DELETE checks `is_cancelled` at row-loop checkpoints
/// and bails with `EngineError::Cancelled`. The host
/// (`spg-server`) creates an `AtomicBool` per query, spawns a
/// watchdog thread that sets it after `SPG_QUERY_TIMEOUT_MS`,
/// and passes it via `execute_with_cancel` / `execute_readonly_with_cancel`.
///
/// `CancelToken::none()` is a no-op — used by the legacy `execute`
/// and `execute_readonly` entry points so existing callers don't
/// change.
/// v7.17.0 Phase 2.3 — monotonic time source for deadline-aware
/// cancellation (PG `statement_timeout`). Returns microseconds
/// since some host-stable monotonic origin (typically the first
/// call into `Instant::now()` on the server). The engine never
/// calls `Instant::now()` directly so the crate stays `#![no_std]`.
pub type MonotonicNowFn = fn() -> u64;

#[derive(Debug, Clone, Copy)]
struct Deadline {
    now_fn: MonotonicNowFn,
    /// Absolute deadline in `now_fn()` units (microseconds).
    deadline_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CancelToken<'a> {
    flag: Option<&'a core::sync::atomic::AtomicBool>,
    // v7.17.0 Phase 2.3 — when set, every existing `cancel.check()`
    // checkpoint also fires `EngineError::Cancelled` once
    // `(now_fn)() >= deadline_us`. No new check sites, no thread
    // spawn per query — the monotonic now-fn read is a vDSO
    // `clock_gettime(CLOCK_MONOTONIC)` (~20ns) and only runs when
    // the host actually wired a deadline (statement_timeout > 0).
    deadline: Option<Deadline>,
}

impl<'a> CancelToken<'a> {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            flag: None,
            deadline: None,
        }
    }

    #[must_use]
    pub const fn from_flag(f: &'a core::sync::atomic::AtomicBool) -> Self {
        Self {
            flag: Some(f),
            deadline: None,
        }
    }

    /// v7.17.0 Phase 2.3 — attach a monotonic deadline. `now_fn`
    /// must return microseconds since a stable origin; the token
    /// trips when `now_fn() >= deadline_us`. Compose with
    /// `from_flag(...)` when both a watchdog flag and a per-statement
    /// timeout are in play (e.g. server-wide `SPG_QUERY_TIMEOUT_MS`
    /// plus session `statement_timeout`); the tighter of the two
    /// wins by virtue of either signaling first.
    #[must_use]
    pub const fn with_deadline(mut self, now_fn: MonotonicNowFn, deadline_us: u64) -> Self {
        self.deadline = Some(Deadline {
            now_fn,
            deadline_us,
        });
        self
    }

    #[must_use]
    pub fn is_cancelled(self) -> bool {
        if self
            .flag
            .is_some_and(|f| f.load(core::sync::atomic::Ordering::Relaxed))
        {
            return true;
        }
        // Deadline check is the second branch so the "no timeout"
        // hot path (`deadline: None`) elides the now-fn call —
        // predicted-not-taken on the SLO INSERT loop.
        if let Some(d) = self.deadline
            && (d.now_fn)() >= d.deadline_us
        {
            return true;
        }
        false
    }

    /// Returns `Err(Cancelled)` if the token has been tripped.
    /// Used at row-loop checkpoints to bail cooperatively without
    /// scattering raw `is_cancelled` checks across the executor.
    #[inline]
    pub fn check(self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// v4.41.1 opaque transaction handle. Returned by `Engine::alloc_tx_id`,
/// threaded through `Engine::execute_in` so dispatch can identify which
/// in-flight TX a statement belongs to. `IMPLICIT_TX` is the reserved
/// slot every legacy caller — engine self-tests, spg-cli, spg-embedded,
/// startup replay — implicitly uses through the unchanged
/// `Engine::execute(sql)` API. v4.41.1 keeps at most one active slot at
/// runtime (dispatch holds `engine.write()` across the wrap, same as
/// v4.34); the map shape is here to let v4.42 turn on N in-flight
/// implicit TXs without reshuffling the engine internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u64);

/// Reserved slot used by `Engine::execute(sql)` — the legacy single-
/// global-shadow path. New `alloc_tx_id` handles start at 1.
pub const IMPLICIT_TX: TxId = TxId(0);

/// v6.7.3 — default segment-size threshold used by `COMPACT COLD
/// SEGMENTS` when no explicit target is supplied. Segments whose
/// `OwnedSegment::bytes().len()` is **strictly** less than this
/// value are eligible to merge. spg-server reads
/// `SPG_COMPACTION_TARGET_SEGMENT_BYTES` to override.
pub const COMPACTION_TARGET_DEFAULT_BYTES: u64 = 4 * 1024 * 1024;

/// Per-slot transaction state. Held inside `tx_catalogs[tx_id]` for the
/// lifetime of a BEGIN..COMMIT (or BEGIN..ROLLBACK) window. Drops when
/// the TX commits (its `catalog` is moved over `Engine.catalog`) or
/// rolls back (slot removed, catalog discarded).
#[derive(Debug, Default, Clone)]
struct TxState {
    /// The TX's shadow copy of the catalog. Started as a clone of
    /// `Engine.catalog` at BEGIN time; writes flow into it; COMMIT
    /// installs it over `Engine.catalog`. `Catalog::clone()` is O(1)
    /// since v4.40 (`PersistentVec` rows + `PersistentBTreeMap` indices).
    catalog: Catalog,
    /// Per-TX savepoint stack. Each entry pairs the savepoint name with
    /// a clone of `catalog` at the moment `SAVEPOINT <name>` fired.
    /// `ROLLBACK TO <name>` restores from the entry and pops everything
    /// after it; `RELEASE <name>` discards the entry and everything
    /// after; COMMIT/ROLLBACK clears the whole stack.
    savepoints: Vec<(String, Catalog)>,
}

/// v7.11.0 — frozen read-only view of the engine's committed state.
/// Constructed via [`Engine::clone_snapshot`]. Holds clones of the
/// catalog, statistics, clock function, and row-cap config — the
/// four fields the `execute_readonly` path actually reads. Cheap to
/// `Clone` (each clone shares the underlying `PersistentVec` row
/// storage; only the trie root pointers copy). Send + Sync so a
/// snapshot can be moved across `tokio::task::spawn_blocking`
/// boundaries without coordination.
///
/// The contract: a snapshot reflects the engine's state at the
/// moment `clone_snapshot()` returned. Subsequent writes to the
/// engine are NOT visible. Callers who need fresher data take a
/// new snapshot.
#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    catalog: Catalog,
    statistics: statistics::Statistics,
    clock: Option<ClockFn>,
    max_query_rows: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Engine {
    /// Committed catalog — what survives `Engine::snapshot()` and what
    /// outside-TX `SELECT`s read.
    catalog: Catalog,
    /// Active TX slots, keyed by `TxId`. Empty when no TX is in flight.
    /// v4.41.1 runtime invariant: at most one entry (single-writer
    /// model unchanged). v4.42 will let dispatch hold multiple entries
    /// concurrently for group commit + engine MVCC.
    tx_catalogs: BTreeMap<TxId, TxState>,
    /// Which slot the next exec_* call should mutate. Set by
    /// `execute_in(sql, tx_id)` at the entry point; legacy `execute(sql)`
    /// sets it to `IMPLICIT_TX`. None when no TX is in flight (read /
    /// write goes straight against `catalog`).
    current_tx: Option<TxId>,
    /// Monotonic counter for `alloc_tx_id`. Starts at 1 — slot 0 is
    /// reserved for `IMPLICIT_TX`.
    next_tx_id: u64,
    /// v7.22 (round-13 T3) — session string-literal dialect. `false`
    /// (default) = PG semantics (backslash literal, `''` escape);
    /// `true` = MySQL semantics (`\'` etc.). Flipped by the
    /// deterministic session signals each dump emits: `SET sql_mode`
    /// (only MySQL clients/dumps send it) turns it on,
    /// `SET standard_conforming_strings = on` (every pg_dump
    /// preamble) turns it off. The plan cache is cleared on every
    /// flip — the same SQL text lexes differently per dialect.
    backslash_escapes: bool,
    /// Optional wall clock used to satisfy `NOW()` / `CURRENT_TIMESTAMP`
    /// / `CURRENT_DATE`. Set by the host environment.
    clock: Option<ClockFn>,
    /// v4.1 cryptographic RNG for per-user password salt. Set by the
    /// host. `None` means SQL-driven `CREATE USER` uses a
    /// deterministic fallback — see `SaltFn`.
    salt_fn: Option<SaltFn>,
    /// v4.2 per-query row cap. `None` = unlimited. When set, a
    /// SELECT that materialises more than `n` rows returns
    /// `EngineError::RowLimitExceeded`. Enforced before the result
    /// is shaped into wire frames so a runaway scan can't blow the
    /// server's heap.
    max_query_rows: Option<usize>,
    /// v7.30.3 (mailrs round-26) per-query byte cap on join/filter
    /// materialisation. `None` = unlimited. Approximate net
    /// accounting (Value heap payloads + per-cell enum overhead)
    /// charged at every point the join pipeline clones rows;
    /// crossing the cap raises `EngineError::QueryBytesExceeded`
    /// instead of pressuring the host into reclaim livelock. The
    /// host wires this to `SPG_MAX_QUERY_BYTES` (embed defaults it
    /// ON; the server keeps its allocator-precise budget as the
    /// outer layer).
    pub(crate) max_query_bytes: Option<usize>,
    /// v4.1 RBAC user table. Empty means "no RBAC configured yet" —
    /// the server decides what that means at the auth boundary
    /// (open mode vs legacy single-password mode). User CRUD goes
    /// through `create_user`/`drop_user`/`verify_user`; persistence
    /// rides the snapshot envelope alongside the catalog.
    pub(crate) users: UserStore,
    /// v6.1.2 logical-replication publication catalog. Empty until
    /// `CREATE PUBLICATION` runs. Persistence rides the v3 envelope
    /// trailer (see `build_envelope`).
    publications: publications::Publications,
    /// v6.1.4 logical-replication subscription catalog. Empty until
    /// `CREATE SUBSCRIPTION` runs. Persistence rides the v4 envelope
    /// trailer.
    subscriptions: subscriptions::Subscriptions,
    /// v6.2.0 — per-column statistics for the cost-based optimizer.
    /// Populated by `ANALYZE`; queried via `spg_statistic` virtual
    /// table. Persistence rides the v5 envelope trailer.
    statistics: statistics::Statistics,
    /// v6.3.0 — engine-level plan cache. Caches the post-`prepare()`
    /// `Statement` keyed on SQL text. In-memory only — does NOT ride
    /// the snapshot envelope (rebuilt on demand after restart).
    plan_cache: plan_cache::PlanCache,
    /// v6.5.1 — per-distinct-SQL execution stats. In-memory only,
    /// surfaced via `spg_stat_query` virtual table. Updated by the
    /// `execute_*` paths after a successful execute.
    query_stats: query_stats::QueryStats,
    /// v6.5.2 — connection-state provider callback. spg-server
    /// registers a function at startup that snapshots its
    /// per-pgwire-connection registry into `ActivityRow`s; engine
    /// reads through it on every `SELECT * FROM spg_stat_activity`.
    /// `None` ⇒ no-data (returns empty rows; matches the no_std
    /// embedded callers that don't run pgwire).
    activity_provider: Option<ActivityProvider>,
    /// v6.5.3 — audit-chain provider + verifier. Same pattern as
    /// activity_provider: spg-server registers both at startup;
    /// engine reads through on `SELECT * FROM spg_audit_chain` and
    /// `SELECT * FROM spg_audit_verify`. `None` ⇒ no-data.
    audit_chain_provider: Option<AuditChainProvider>,
    audit_verifier: Option<AuditVerifier>,
    /// v6.5.6 — slow-query log threshold in microseconds. When set,
    /// every successful execute whose elapsed exceeds the threshold
    /// gets fed to the registered slow-query log callback (so
    /// spg-server can emit a structured log line). Default `None`
    /// = no slow-query logging.
    slow_query_threshold_us: Option<u64>,
    slow_query_logger: Option<SlowQueryLogger>,
    /// v7.12.1 — session parameters set via `SET <name> = <value>`.
    /// Only `default_text_search_config` is consumed by the engine
    /// today (the FTS function dispatcher reads it when
    /// `to_tsvector(text)` is called without an explicit config).
    /// All other names are accepted + recorded so PG-dump output
    /// loads, but have no behavioural effect.
    pub(crate) session_params: BTreeMap<String, String>,
    /// v7.12.7 — depth counter for trigger-emitted embedded SQL.
    /// Each time the engine executes a `DeferredEmbeddedStmt` it
    /// increments this; the recursive `execute_stmt_with_cancel`
    /// inside that path checks against [`MAX_TRIGGER_RECURSION`]
    /// to bound runaway cascades (trigger A's UPDATE on table B
    /// fires trigger B which UPDATEs table A which fires trigger
    /// A again…). Reset to 0 once the original DML returns.
    trigger_recursion_depth: u32,
    /// v7.14.0 — when `SET FOREIGN_KEY_CHECKS=0` is in effect
    /// (mysqldump preamble), the FK existence + arity check at
    /// CREATE TABLE time is deferred. FKs referencing a
    /// not-yet-existing parent land in `pending_foreign_keys`
    /// keyed by child table; `SET FOREIGN_KEY_CHECKS=1` drains
    /// the queue and resolves each FK against the now-complete
    /// catalog. Empty by default; the queue is drained on every
    /// `RESET ALL` too.
    foreign_key_checks: bool,
    /// v7.16.2 — true on the temp Engine an outer
    /// `exec_select_with_meta_views` builds, telling that
    /// temp engine "stop short-circuiting into the meta-view
    /// path — your catalog already has the materialised
    /// tables; just run the regular SELECT." Without this we'd
    /// infinite-loop since the meta-view name (e.g.
    /// `__spg_info_columns`) still triggers
    /// `select_references_meta_view`.
    meta_views_materialised: bool,
    pending_foreign_keys: Vec<(alloc::string::String, spg_sql::ast::ForeignKeyConstraint)>,
}

/// v7.12.7 — hard cap on nested trigger-emitted embedded SQL
/// fires. 16 deep is well past anything a normal trigger graph
/// uses while still preventing infinite-loop wedging.
const MAX_TRIGGER_RECURSION: u32 = 16;

/// v6.5.6 — callback signature for slow-query log emission. Called
/// with `(sql, elapsed_us)` once per successful execute that crosses
/// the threshold.
pub type SlowQueryLogger = fn(&str, u64);

/// v6.5.2 — one row of `spg_stat_activity`. Engine-public so
/// spg-server can construct rows without re-exporting internal
/// dispatch types.
#[derive(Debug, Clone)]
pub struct ActivityRow {
    pub pid: u32,
    pub user: String,
    pub started_at_us: i64,
    pub current_sql: String,
    pub wait_event: String,
    pub elapsed_us: i64,
    pub in_transaction: bool,
    /// v7.17 Phase 2.4 — startup-param `application_name` (or the
    /// last value the client sent via `SET application_name = '...'`).
    /// Empty when the client never declared one.
    pub application_name: String,
}

/// v6.5.2 — provider callback type. Fresh snapshot returned each
/// call; engine doesn't cache the slice.
pub type ActivityProvider = fn() -> Vec<ActivityRow>;

/// v6.5.3 — one row of `spg_audit_chain`. Engine-public so
/// spg-server can construct rows directly from `AuditEntry`.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub seq: i64,
    pub ts_ms: i64,
    pub prev_hash_hex: String,
    pub entry_hash_hex: String,
    pub sql: String,
}

/// v6.5.3 — chain-table provider + verifier. spg-server registers
/// fn pointers that snapshot / verify the audit log. `verify`
/// returns `(verified_count, broken_at_seq)` — `broken_at_seq` is
/// `-1` on a clean chain.
pub type AuditChainProvider = fn() -> Vec<AuditRow>;
pub type AuditVerifier = fn() -> (i64, i64);

impl Engine {
    pub fn new() -> Self {
        Self {
            catalog: Catalog::new(),
            tx_catalogs: BTreeMap::new(),
            current_tx: None,
            backslash_escapes: false,
            next_tx_id: 1,
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            max_query_bytes: None,
            users: UserStore::new(),
            publications: publications::Publications::new(),
            subscriptions: subscriptions::Subscriptions::new(),
            statistics: statistics::Statistics::new(),
            plan_cache: plan_cache::PlanCache::new(),
            query_stats: query_stats::QueryStats::new(),
            activity_provider: None,
            audit_chain_provider: None,
            audit_verifier: None,
            slow_query_threshold_us: None,
            slow_query_logger: None,
            session_params: BTreeMap::new(),
            trigger_recursion_depth: 0,
            foreign_key_checks: true,
            meta_views_materialised: false,
            pending_foreign_keys: Vec::new(),
        }
    }

    /// v7.11.0 — clone the engine's committed catalog + read-time
    /// state into a frozen `CatalogSnapshot`. Cheap (`Catalog` is
    /// backed by `PersistentVec`; cloning is O(log n) per table).
    /// Subsequent writes to this engine are invisible to the
    /// snapshot; the snapshot is self-contained and can be moved
    /// to another thread for concurrent `execute_readonly_on_snapshot`
    /// calls. The basis for [`AsyncReadHandle`] in spg-embedded-tokio
    /// and any other read-fanout pattern.
    #[must_use]
    pub fn clone_snapshot(&self) -> CatalogSnapshot {
        CatalogSnapshot {
            catalog: self.active_catalog().clone(),
            statistics: self.statistics.clone(),
            clock: self.clock,
            max_query_rows: self.max_query_rows,
        }
    }

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

    /// Construct an engine restored from a previously-snapshotted catalog
    /// (see `snapshot()`).
    pub fn restore(catalog: Catalog) -> Self {
        Self {
            catalog,
            tx_catalogs: BTreeMap::new(),
            current_tx: None,
            backslash_escapes: false,
            next_tx_id: 1,
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            max_query_bytes: None,
            users: UserStore::new(),
            publications: publications::Publications::new(),
            subscriptions: subscriptions::Subscriptions::new(),
            statistics: statistics::Statistics::new(),
            plan_cache: plan_cache::PlanCache::new(),
            query_stats: query_stats::QueryStats::new(),
            activity_provider: None,
            audit_chain_provider: None,
            audit_verifier: None,
            slow_query_threshold_us: None,
            slow_query_logger: None,
            session_params: BTreeMap::new(),
            trigger_recursion_depth: 0,
            foreign_key_checks: true,
            meta_views_materialised: false,
            pending_foreign_keys: Vec::new(),
        }
    }

    /// Restore an engine + user table from a v4.1 envelope produced
    /// by `snapshot_with_users()`. Falls back to plain catalog-only
    /// restore if the envelope magic isn't present (so v3.x snapshot
    /// files still load). v6.1.2 adds the optional publications
    /// trailer (envelope v3); a v1/v2 envelope deserialises to an
    /// empty publication table.
    pub fn restore_envelope(buf: &[u8]) -> Result<Self, EngineError> {
        match split_envelope(buf) {
            EnvelopeParse::Pair {
                catalog: catalog_bytes,
                users: user_bytes,
                publications: pub_bytes,
                subscriptions: sub_bytes,
                statistics: stats_bytes,
            } => {
                let catalog = Catalog::deserialize(catalog_bytes).map_err(EngineError::Storage)?;
                let users = users::deserialize_users(user_bytes)
                    .map_err(|e| EngineError::Unsupported(alloc::format!("users restore: {e}")))?;
                let publications = match pub_bytes {
                    Some(b) => publications::Publications::deserialize(b).map_err(|e| {
                        EngineError::Unsupported(alloc::format!("publications restore: {e:?}"))
                    })?,
                    None => publications::Publications::new(),
                };
                let subscriptions = match sub_bytes {
                    Some(b) => subscriptions::Subscriptions::deserialize(b).map_err(|e| {
                        EngineError::Unsupported(alloc::format!("subscriptions restore: {e:?}"))
                    })?,
                    None => subscriptions::Subscriptions::new(),
                };
                let statistics = match stats_bytes {
                    Some(b) => statistics::Statistics::deserialize(b).map_err(|e| {
                        EngineError::Unsupported(alloc::format!("statistics restore: {e:?}"))
                    })?,
                    None => statistics::Statistics::new(),
                };
                Ok(Self {
                    catalog,
                    tx_catalogs: BTreeMap::new(),
                    current_tx: None,
                    backslash_escapes: false,
                    next_tx_id: 1,
                    clock: None,
                    salt_fn: None,
                    max_query_rows: None,
                    max_query_bytes: None,
                    users,
                    publications,
                    subscriptions,
                    statistics,
                    plan_cache: plan_cache::PlanCache::new(),
                    query_stats: query_stats::QueryStats::new(),
                    activity_provider: None,
                    audit_chain_provider: None,
                    audit_verifier: None,
                    slow_query_threshold_us: None,
                    slow_query_logger: None,
                    session_params: BTreeMap::new(),
                    trigger_recursion_depth: 0,
                    foreign_key_checks: true,
                    meta_views_materialised: false,
                    pending_foreign_keys: Vec::new(),
                })
            }
            EnvelopeParse::CrcMismatch { expected, computed } => {
                Err(EngineError::Storage(StorageError::Corrupt(alloc::format!(
                    "snapshot envelope CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
                ))))
            }
            EnvelopeParse::Bare => {
                let catalog = Catalog::deserialize(buf).map_err(EngineError::Storage)?;
                Ok(Self::restore(catalog))
            }
        }
    }

    pub const fn users(&self) -> &UserStore {
        &self.users
    }

    /// Builder: attach a wall clock so `NOW()` / `CURRENT_TIMESTAMP` /
    /// `CURRENT_DATE` evaluate to a real value instead of erroring out.
    #[must_use]
    pub const fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Builder: attach an OS-backed RNG for per-user password salts.
    /// The host (`spg-server`) typically wires this to `/dev/urandom`.
    #[must_use]
    pub const fn with_salt_fn(mut self, f: SaltFn) -> Self {
        self.salt_fn = Some(f);
        self
    }

    /// Builder: cap the number of rows a single SELECT may return.
    /// Exceeding the cap raises `EngineError::RowLimitExceeded` —
    /// the bound is checked inside the executor so a runaway
    /// catalog scan can't allocate millions of rows before the
    /// server gets a chance to reject the result.
    #[must_use]
    pub const fn with_max_query_rows(mut self, n: usize) -> Self {
        self.max_query_rows = Some(n);
        self
    }

    /// Builder: cap the approximate heap bytes a single SELECT's
    /// join/filter materialisation may hold. Exceeding the cap
    /// raises `EngineError::QueryBytesExceeded`. Rows are the wrong
    /// unit when one row carries a multi-MB body (mailrs round-26:
    /// 1000-row batches of full mail text walked a 15 GiB host into
    /// reclaim livelock without ever tripping a row ceiling).
    #[must_use]
    pub const fn with_max_query_bytes(mut self, n: usize) -> Self {
        self.max_query_bytes = Some(n);
        self
    }

    /// The *committed* catalog. Note: during a transaction this returns the
    /// pre-TX state — `SELECT` inside a TX goes through `execute()` and reads
    /// the shadow. Tests that inspect outside-TX state should use this.
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Serialize the *committed* catalog to bytes. v0.6 was full-snapshot; v0.9
    /// adds the rule that an open TX's shadow is never snapshotted — only the
    /// post-COMMIT state is persisted. v4.1 wraps the catalog in an envelope
    /// when there are users to persist; an empty user table snapshots as the
    /// bare catalog format (backwards-compat with v3.x readers). v6.1.2
    /// adds publications to the envelope condition: either non-empty
    /// users OR non-empty publications now triggers the envelope path.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.users.is_empty()
            && self.publications.is_empty()
            && self.subscriptions.is_empty()
            && self.statistics.is_empty()
        {
            self.catalog.serialize()
        } else {
            build_envelope(
                &self.catalog.serialize(),
                &users::serialize_users(&self.users),
                &self.publications.serialize(),
                &self.subscriptions.serialize(),
                &self.statistics.serialize(),
            )
        }
    }

    /// True when at least one TX slot is in flight. v4.41.1 runtime
    /// invariant: at most one slot active at a time (dispatch holds
    /// `engine.write()` across the entire wrap). v4.42 will let this
    /// return true with multiple slots concurrently.
    pub fn in_transaction(&self) -> bool {
        !self.tx_catalogs.is_empty()
    }

    /// v4.41.1 allocate a fresh TX handle. Used by spg-server dispatch
    /// to scope each implicit-wrap BEGIN..stmt..COMMIT to its own slot
    /// in `tx_catalogs`. v4.42 — the commit-barrier leader allocates
    /// one of these per task in its group, runs `BEGIN`+sql+`COMMIT`
    /// sequentially under a single `engine.write()` so each task's
    /// mutations accumulate into shared state, then either keeps the
    /// accumulated state (fsync OK) or restores the pre-image via
    /// `replace_catalog` (fsync err).
    pub fn alloc_tx_id(&mut self) -> TxId {
        let id = TxId(self.next_tx_id);
        self.next_tx_id = self.next_tx_id.saturating_add(1);
        id
    }

    /// v4.42 — atomically replace the live catalog. Used by the
    /// commit-barrier leader to roll back a group whose batched
    /// fsync failed: the leader snapshots `engine.catalog().clone()`
    /// (O(1) Arc bump after the v4.39/v4.40 persistent migration)
    /// at group start, sequentially applies each task's BEGIN+sql+
    /// COMMIT under the same write lock to accumulate mutations
    /// into shared state, batches the WAL bytes, fsyncs once, and
    /// on failure calls this with the pre-image to undo every
    /// task in the group at once.
    ///
    /// **Does NOT touch `tx_catalogs` / `current_tx`.** Any
    /// explicit-TX slot from a concurrent client (created via the
    /// legacy `IMPLICIT_TX`-less dispatch path or via the future
    /// MVCC-readers v5+ work) has its own snapshot baked into the
    /// slot — restoring `self.catalog` to the pre-image leaves
    /// those slots untouched, exactly as they were when the leader
    /// took the lock. The leader's own implicit-TX slots are all
    /// already discarded (`exec_commit` removed them as each
    /// task's COMMIT ran) by the time this is reached.
    pub fn replace_catalog(&mut self, catalog: Catalog) {
        self.catalog = catalog;
    }

    /// v6.7.0 — public shim around `Catalog::freeze_oldest_to_cold`
    /// so tests + the spg-server freezer can drive a freeze without
    /// reaching into the private `active_catalog_mut`. v6.7.4
    /// parallel freezer will build on this surface.
    ///
    /// Marks the table's cached `cold_row_count` stale because the
    /// freeze added cold locators that ANALYZE hasn't yet refreshed.
    pub fn freeze_oldest_to_cold(
        &mut self,
        table_name: &str,
        index_name: &str,
        max_rows: usize,
    ) -> Result<spg_storage::FreezeReport, EngineError> {
        let report = self
            .active_catalog_mut()
            .freeze_oldest_to_cold(table_name, index_name, max_rows)
            .map_err(EngineError::Storage)?;
        if let Some(t) = self.active_catalog_mut().get_mut(table_name) {
            t.mark_cold_row_count_stale();
        }
        Ok(report)
    }

    /// v6.7.5 — public shim used by the spg-server follower's
    /// segment-forwarding receiver. Registers a cold-tier segment
    /// at a specific id (the master's id, as transmitted on the
    /// wire) so the follower's BTree-Cold locators stay byte-
    /// identical with the master's. Wraps
    /// `Catalog::load_segment_bytes_at` under the standard
    /// clone-mutate-replace pattern.
    ///
    /// Returns `Ok(())` on success **and** on the "slot already
    /// occupied" case — a follower mid-reconnect may receive a
    /// segment chunk for a segment_id it already has on disk
    /// (forwarded last session); the caller should treat that
    /// path as a no-op rather than a fatal error.
    pub fn receive_cold_segment(
        &mut self,
        segment_id: u32,
        bytes: Vec<u8>,
    ) -> Result<(), EngineError> {
        let mut new_cat = self.catalog.clone();
        match new_cat.load_segment_bytes_at(segment_id, bytes) {
            Ok(()) => {
                self.replace_catalog(new_cat);
                Ok(())
            }
            Err(StorageError::Corrupt(msg)) if msg.contains("already occupied") => Ok(()),
            Err(e) => Err(EngineError::Storage(e)),
        }
    }

    pub(crate) fn active_catalog(&self) -> &Catalog {
        match self.current_tx {
            Some(t) => self
                .tx_catalogs
                .get(&t)
                .map_or(&self.catalog, |s| &s.catalog),
            None => &self.catalog,
        }
    }

    fn active_catalog_mut(&mut self) -> &mut Catalog {
        let tx = self.current_tx;
        match tx {
            Some(t) => match self.tx_catalogs.get_mut(&t) {
                Some(s) => &mut s.catalog,
                None => &mut self.catalog,
            },
            None => &mut self.catalog,
        }
    }

    /// Read-only execute path. Succeeds for `SELECT` / `SHOW TABLES`
    /// / `SHOW COLUMNS`; returns `EngineError::WriteRequired` for
    /// every other statement, so the caller can fall through to the
    /// `&mut self` `execute` path under a write lock. Engine state is
    /// not mutated even on the success path (`rewrite_clock_calls`
    /// and `resolve_order_by_position` both mutate the locally-owned
    /// AST, not `self`).
    ///
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

    /// v4.2: cap result-set size. Applied after the executor
    /// materialises rows but before they leave the engine — wrapping
    /// every Rows-returning exec_* function would scatter the check.
    ///
    /// v7.31 (memory campaign, bucket A) — the same choke point now
    /// also enforces the BYTE budget on the final result set, so
    /// single-table and aggregate paths (which don't route through
    /// the join materialiser's incremental accounting) still cannot
    /// hand the host an unbounded result. Intermediate single-table
    /// clones are the 7.31.x follow-up (design doc, bucket A).
    fn enforce_row_limit(
        &self,
        result: Result<QueryResult, EngineError>,
    ) -> Result<QueryResult, EngineError> {
        if let Ok(QueryResult::Rows { rows, .. }) = &result {
            if let Some(cap) = self.max_query_rows
                && rows.len() > cap
            {
                return Err(EngineError::RowLimitExceeded(cap));
            }
            if let Some(byte_cap) = self.max_query_bytes
                && approx_rows_bytes(rows) > byte_cap
            {
                return Err(EngineError::QueryBytesExceeded(byte_cap));
            }
        }
        result
    }

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
        let result = self.execute_inner_with_cancel(sql, cancel);
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

    fn execute_stmt_with_cancel(
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

/// v7.31 (memory campaign — ceiling-first / never-die, design v1) —
/// per-table slice of the engine's resident-memory accounting.
/// `hot_encoded_bytes` is the storage layer's maintained meter (what
/// the rows encode to); `approx_resident_bytes` is what they COST in
/// RAM (per-cell enum slots + heap payloads via `approx_row_bytes`)
/// — the gap between the two is the representation multiplier the
/// round-26 report measured at ~11× end-to-end.
#[derive(Debug, Clone)]
pub struct TableMemoryStats {
    pub name: String,
    pub hot_rows: u64,
    /// Cached cold-row count (refreshed by ANALYZE — see
    /// `Table::cold_row_count`'s staleness contract).
    pub cold_rows: u64,
    pub hot_encoded_bytes: u64,
    pub approx_resident_bytes: u64,
    pub index_count: u64,
    /// BTree indices are walked entry-by-entry (operator surface,
    /// not a hot path); NSW graphs and BRIN are parametric
    /// ESTIMATES until spg-storage carries its own byte meters
    /// (7.31.x follow-up in the design doc).
    pub approx_index_bytes: u64,
}

/// v7.31 — whole-engine memory snapshot: the polling form of the
/// round-26 ask-4 watermark signal. Hosts compare
/// `total_approx_resident_bytes` (+ their own WAL/file accounting)
/// against their deployment ceiling and shed/shrink before the
/// kernel does it for them.
#[derive(Debug, Clone)]
pub struct MemoryStats {
    pub tables: Vec<TableMemoryStats>,
    pub total_hot_encoded_bytes: u64,
    pub total_approx_resident_bytes: u64,
    pub total_approx_index_bytes: u64,
    /// The active per-query materialisation budget (bucket A), so a
    /// monitoring host sees ceiling and usage through one call.
    pub max_query_bytes: Option<usize>,
}

/// v6.2.0 — true for engine-managed catalog tables that the bare
/// `ANALYZE` (no target) should skip. v6.2.0 has no internal
/// tables yet (publications / subscriptions / users / statistics
/// all live as engine fields, not catalog tables), so this is a
/// reserved future-proofing hook — every existing user table is
/// analysed.
const fn is_internal_table_name(_name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests;
