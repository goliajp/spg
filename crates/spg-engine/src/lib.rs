//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
mod bytebudget;
mod cancel;
mod clock;
mod constraints;
mod conversions;
pub mod copy;
mod ddl;
pub mod describe;
mod dml;
mod envelope;
pub mod eval;
mod execute;
mod explain;
mod expr_analysis;
pub mod fts;
mod index_access;
mod join;
mod joinfold;
pub mod json;
mod maintenance;
pub mod memoize;
mod numeric;
mod orderby;
pub mod plan_cache;
mod plpgsql;
pub mod publications;
pub mod query_stats;
mod readonly;
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
pub use cancel::{CancelToken, MonotonicNowFn};
pub use execute::StreamItem;

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
pub use subquery::{
    BATCHED_SCALAR_FALL_THROUGH_COUNT, BATCHED_SCALAR_KEYED_FIRE_COUNT,
    BATCHED_SCALAR_KEYED_PROBE_COUNT, PULLUP_LIMIT1_FIRE_COUNT,
};
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

// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::Statement as ParsedStatement;
use spg_sql::parser::ParseError;
use spg_storage::{Catalog, ColumnSchema, Row, RowChange, StorageError};

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

/// CoW-1 (v7.34) — frozen view of the *persisted* committed engine
/// state. Carries every field the `snapshot()` envelope serializes;
/// `Clone` is O(1) on the catalog (Arc bump) and cheap typed-clones
/// on the trailers. Decouples "capture state" from "serialize bytes"
/// so the background-checkpoint worker can hold the snapshot and
/// produce bytes off the engine write lock.
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    catalog: Catalog,
    users: UserStore,
    publications: publications::Publications,
    subscriptions: subscriptions::Subscriptions,
    statistics: statistics::Statistics,
}

impl EngineSnapshot {
    /// Same envelope rules as `Engine::snapshot()`: bare catalog when
    /// every trailer is empty, full envelope otherwise.
    pub fn serialize(&self) -> Vec<u8> {
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
}

// The engine carries several independent session/capture flags (dialect,
// FK-checks, meta-view materialisation, redo capture); they're orthogonal
// switches, not a state enum begging to be modelled.
#[allow(clippy::struct_excessive_bools)]
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
    /// v7.34 (crash-recovery P0 #2) — row-level redo capture. When the
    /// embedding layer turns this on (persistence enabled), each mutating
    /// `execute` records the physical [`RowChange`]s it applied; the
    /// engine drains them into `last_redo` on success, and the embedded
    /// layer reads them via [`Engine::take_redo`] to write the WAL in
    /// place of the SQL text. Off (default) = zero capture overhead.
    redo_capture: bool,
    /// Redo captured by the most recent successful mutating `execute`,
    /// awaiting drain by the embedding layer. Cleared on each capture.
    last_redo: Vec<RowChange>,
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
            redo_capture: false,
            last_redo: Vec::new(),
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
            redo_capture: false,
            last_redo: Vec::new(),
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
                    redo_capture: false,
                    last_redo: Vec::new(),
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

    /// Capture a frozen view of the committed engine state. Catalog
    /// is O(1) Arc bump; trailers are cheap clones. Decouples "capture"
    /// (needs &Engine) from "serialize" (CPU, no engine access) — the
    /// seam the background-checkpoint worker rides in CoW-2.
    pub fn snapshot_data(&self) -> EngineSnapshot {
        EngineSnapshot {
            catalog: self.catalog.clone(),
            users: self.users.clone(),
            publications: self.publications.clone(),
            subscriptions: self.subscriptions.clone(),
            statistics: self.statistics.clone(),
        }
    }

    /// Serialize the *committed* catalog to bytes. v0.6 was full-snapshot; v0.9
    /// adds the rule that an open TX's shadow is never snapshotted — only the
    /// post-COMMIT state is persisted. v4.1 wraps the catalog in an envelope
    /// when there are users to persist; an empty user table snapshots as the
    /// bare catalog format (backwards-compat with v3.x readers). v6.1.2
    /// adds publications to the envelope condition: either non-empty
    /// users OR non-empty publications now triggers the envelope path.
    pub fn snapshot(&self) -> Vec<u8> {
        self.snapshot_data().serialize()
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

    /// v7.34 (crash-recovery P0 #2) — turn row-level redo capture on/off.
    /// The embedding layer enables it when persistence is on so each
    /// mutating `execute` records the physical [`RowChange`]s it applied
    /// (drained via [`Engine::take_redo`]). Off = zero capture overhead.
    pub fn set_redo_capture(&mut self, on: bool) {
        self.redo_capture = on;
    }

    /// v7.34 — take the redo captured by the most recent successful
    /// mutating `execute` (empty when capture is off, the statement was a
    /// read, or it changed nothing). The embedding layer writes these to
    /// the WAL in place of the SQL text.
    pub fn take_redo(&mut self) -> Vec<RowChange> {
        core::mem::take(&mut self.last_redo)
    }

    /// v7.34 (crash-recovery P0 #2) — replay a row-level redo log onto the
    /// committed catalog (the row-level WAL recovery primitive: apply the
    /// captured physical changes from a checkpoint baseline, in place of
    /// re-executing the SQL). Trusts the log — no uniqueness/FK/parse.
    pub fn apply_redo(&mut self, changes: &[RowChange]) -> Result<(), EngineError> {
        self.catalog
            .apply_redo(changes)
            .map_err(EngineError::Storage)
    }

    /// Read-only execute path. Succeeds for `SELECT` / `SHOW TABLES`
    /// / `SHOW COLUMNS`; returns `EngineError::WriteRequired` for
    /// every other statement, so the caller can fall through to the
    /// `&mut self` `execute` path under a write lock. Engine state is
    /// not mutated even on the success path (`rewrite_clock_calls`
    /// and `resolve_order_by_position` both mutate the locally-owned
    /// AST, not `self`).
    ///
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
    /// v7.31 C2 — sum of `IndexKind::approx_resident_bytes()` over the
    /// table's indices: every variant (BTree / NSW / BRIN / GIN family)
    /// walks its own structure, so the GIN posting lists and NSW layer
    /// adjacency that dominate text/vector tables are counted honestly
    /// instead of the old flat-token estimate.
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
    /// v7.31 C2 — bucket D: live WAL bytes (active chunk + buffered,
    /// uncheckpointed). `None` from the engine itself — it has no WAL;
    /// the durable hosts (embed `Database`, server) fill it in from
    /// their own WAL accounting. `Some(0)` means "host has a WAL and
    /// it is empty"; `None` means "no WAL on this path" (in-memory).
    pub wal_bytes: Option<u64>,
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
