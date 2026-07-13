//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

// v7.37.9 T3 — `bump_counter!(C)` / `bump_counter!(C, N)` macros for the
// Step VM + aggregate hot-path diagnostic counters. Gated on the
// `perf-counters` feature so release builds pay zero cost; the
// `xtests/dogfood_replay/spg-counter-dump` binary turns the feature on
// to attribute Class A / B / C cascade cost.
#[cfg(not(feature = "perf-counters"))]
#[macro_export]
macro_rules! bump_counter {
    ($c:path) => {{
        let _ = &$c;
    }};
    ($c:path, $n:expr) => {{
        let _ = &$c;
        let _ = &$n;
    }};
}

#[cfg(feature = "perf-counters")]
#[macro_export]
macro_rules! bump_counter {
    ($c:path) => {{
        $c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }};
    ($c:path, $n:expr) => {{
        $c.fetch_add($n, core::sync::atomic::Ordering::Relaxed);
    }};
}

pub mod aggregate;
pub(crate) mod amcheck;
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
mod join_using;
mod joinfold;
pub mod json;
pub mod locks;
mod maintenance;
pub mod memoize;
mod numeric;
mod orderby;
mod partition;
pub(crate) mod partition_walks;
pub mod plan_cache;
mod plpgsql;
pub mod publications;
pub mod query_stats;
mod readonly;
pub mod reorder;
mod rls;
pub mod scalarsq_streaming;
mod select;
pub mod selectivity;
mod sequence;
mod session;
mod show;
mod spg_admin;
pub mod statistics;
pub mod subquery;
pub mod subscriptions;
mod substitute;
mod system_catalog;
mod table_access;
pub mod testkit;
mod transaction;
pub(crate) use transaction::{TxStmtClass, classify_stmt_for_tx};
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
    format_bigint_2d_text_pub, format_bit_string, format_circle, format_hstore_text, format_inet,
    format_int_2d_text_pub, format_line, format_lseg, format_macaddr, format_macaddr8,
    format_multirange, format_path, format_pg_box, format_pg_lsn, format_point, format_polygon,
    format_range_text, format_text_2d_text_pub,
};
pub(crate) use ddl::{
    canonicalize_set_value, enforce_enum_label, eval_runtime_default_free,
    resolve_column_default_free,
};
pub(crate) use envelope::{EnvelopeParse, build_envelope, split_envelope};
use expr_analysis::*;
use index_access::*;
pub use join::{ANTI_JOIN_FAST_PATH_FIRED, ANTI_JOIN_FAST_PATH_TRIED};
pub(crate) use orderby::{
    OrderKey, apply_offset_and_limit, apply_offset_and_limit_tagged, build_order_keys,
    canonical_value_repr, cmp_multi_key, expand_group_by_all, order_by_value_cmp,
    partial_sort_tagged, render_histogram_bounds, resolve_order_by_position, sort_by_keys,
    sort_values_for_histogram, topk_trim, value_cmp, value_to_f64,
};
pub(crate) use select::{build_projection, infer_column_types, value_to_order_key};
pub(crate) use show::render_create_table;
pub use subquery::{
    BATCHED_SCALAR_FALL_THROUGH_COUNT, BATCHED_SCALAR_KEYED_FIRE_COUNT,
    BATCHED_SCALAR_KEYED_PROBE_COUNT, EXISTS_BATCH_FALL_THROUGH_COUNT, EXISTS_BATCH_FIRE_COUNT,
    EXISTS_PULLUP_BAIL_INNER_FROM, EXISTS_PULLUP_BAIL_INNER_SHAPE,
    EXISTS_PULLUP_BAIL_MULTICOL_DISABLED, EXISTS_PULLUP_BAIL_NO_CORR, EXISTS_PULLUP_BAIL_NO_WHERE,
    EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER, EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING,
    EXISTS_PULLUP_CANDIDATE_COUNT, EXISTS_PULLUP_FIRE_COUNT, EXISTS_PULLUP_MULTICOL_DISABLE,
    PULLUP_LIMIT1_FIRE_COUNT, SCALARSQ_PK_PROBE_FIRED, ScalarPkProbeFastPath,
    expr_tree_has_subquery,
};
pub(crate) use subquery::{build_in_list_set, collect_scalar_subqueries, expr_has_subquery};
pub use substitute::substitute_placeholders;
use substitute::*;
use system_catalog::*;
use window::*;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::{SelectStatement, Statement as ParsedStatement};
// v7.37.15 Phase B — re-export the visibility primitives for engine
// callers so the per-row MVCC types live behind one stable name
// (`spg_engine::Snapshot` / `spg_engine::RowHeader`) instead of every
// caller threading through `spg_storage::snapshot::*` directly.
pub use spg_storage::RowChange;
pub use spg_storage::row_header::{RowHeader, XMAX_ALIVE, XMIN_FROZEN};
pub use spg_storage::snapshot::{
    AllCommitted, InProgressSet, Snapshot, XactStatus, XactStatusOracle,
};

/// v7.37.15 (Phase C.2) — the engine is its own visibility oracle.
/// Scans hold `&Engine` while reading, so a scan site can pass `self`
/// as the [`XactStatusOracle`] alongside its [`Snapshot`] when the
/// visibility gate migrates from `visible` to `visible_with_status`
/// (next Phase C step). Delegates to [`Engine::xact_status`].
impl XactStatusOracle for Engine {
    fn status(&self, version: u64) -> XactStatus {
        self.xact_status(version)
    }
}
// v7.37.14 (A2.5-stub) — re-export the silent-FOR-UPDATE telemetry
// helper through the engine surface so downstream wrappers
// (spg-embedded / spg-embedded-tokio / spgctl) and their tests
// don't need a direct `spg-sql` dep.
use spg_sql::parser::ParseError;
pub use spg_sql::silent_for_update_count;
use spg_storage::{Catalog, ColumnSchema, Row, StorageError};

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
        rows: Vec<Row<'static>>,
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
    /// v7.38 (read01 P3.26) — a statement other than COMMIT / ROLLBACK /
    /// ROLLBACK TO SAVEPOINT was issued after an earlier statement in the
    /// same transaction failed. PG aborts the whole transaction on the
    /// first error and rejects everything until it is ended (SQLSTATE
    /// 25P02); this mirrors that so partial work can't slip through.
    InFailedTransaction,
    /// v7.38 (read01 P4.02) — a scalar / row subquery used as an
    /// expression returned more than one row. PG raises this as
    /// SQLSTATE 21000 (CARDINALITY_VIOLATION) with a fixed message.
    CardinalityViolation,
    /// v7.37.17 (Phase E3) — a REPEATABLE READ / SERIALIZABLE commit
    /// found a write-write conflict with a concurrently-committed
    /// transaction (a row this tx wrote was deleted/updated by another
    /// committed writer, or a unique key this tx inserted was taken).
    /// PG raises SQLSTATE 40001; the client retries the transaction.
    /// The failing COMMIT rolls the transaction back, like PG.
    SerializationFailure(String),
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
    /// v7.38 Epic P (panic isolation): a panic unwound out of
    /// statement execution and was caught at the engine's
    /// `execute_*` boundary (see `execute_in_with_cancel`). The
    /// in-flight transaction's shadow was discarded (rollback) and
    /// the engine left consistent; the caller sees this ordinary
    /// error instead of a crashed process / poisoned lock. NOTE:
    /// under the release `panic = "abort"` profile the process
    /// aborts before any unwind, so this variant only ever surfaces
    /// in dev/test (`panic = "unwind"`) — and in production once a
    /// later slice flips the release profile to unwind.
    Internal(String),
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
            Self::InFailedTransaction => f.write_str(
                "current transaction is aborted, commands ignored until end of transaction block",
            ),
            Self::CardinalityViolation => {
                f.write_str("more than one row returned by a subquery used as an expression")
            }
            Self::SerializationFailure(detail) => {
                write!(
                    f,
                    "could not serialize access due to concurrent update: {detail}"
                )
            }
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
            Self::Internal(s) => write!(f, "internal error: {s}"),
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

/// v7.39 (pg_stat knife A) — host-provided live connection count for
/// `pg_stat_database.numbackends`.
pub type BackendCountFn = fn() -> u32;

/// v7.39 (read01 pgstatfuncs.c) — host-provided identity of the CALLING
/// connection for `pg_backend_pid()` / the pg_stat_activity self-join.
/// The host reads a connection-thread-local set at session start; the
/// no_std engine just calls through. `None` (embedded) → pid 1.
pub type BackendPidFn = fn() -> u32;

/// v7.39 (tz epic) — host-injected IANA timezone lookups (the no_std
/// engine can't read the system zoneinfo directory; spg-tzif is the
/// std-side implementation). All instants are MICROSECONDS.
/// UTC offset (µs east) of a zone at a UTC instant; None = unknown zone.
pub type TzOffsetFn = fn(&str, i64) -> Option<i64>;
/// Local wall-clock µs -> UTC µs with PG's DST disambiguation.
pub type TzLocalizeFn = fn(&str, i64) -> Option<i64>;
/// Canonical zone spelling ("asia/tokyo" -> "Asia/Tokyo").
pub type TzCanonFn = fn(&str) -> Option<alloc::string::String>;
/// Zone designation ("JST", "EDT") at a UTC instant.
pub type TzAbbrevFn = fn(&str, i64) -> Option<alloc::string::String>;

/// v7.39 (tz epic) — per-statement snapshot of the session TimeZone,
/// consumed per-VALUE by the timestamptz renderers (a DST zone's
/// offset depends on the instant being rendered).
#[derive(Debug, Clone)]
pub enum SessionTz {
    Utc,
    /// Fixed offset, µs east.
    Fixed(i64),
    /// IANA zone + the host lookups.
    Named(alloc::string::String, TzOffsetFn, TzAbbrevFn),
}

impl SessionTz {
    #[must_use]
    pub fn is_utc(&self) -> bool {
        matches!(self, Self::Utc) || matches!(self, Self::Fixed(0))
    }

    /// Offset (µs east) at a UTC instant.
    #[must_use]
    pub fn offset_at(&self, utc_micros: i64) -> i64 {
        match self {
            Self::Utc => 0,
            Self::Fixed(off) => *off,
            Self::Named(zone, f, _) => f(zone, utc_micros).unwrap_or(0),
        }
    }

    /// Designation for the non-ISO DateStyle suffix: a named zone's
    /// abbreviation at the instant; None for UTC/fixed (callers spell
    /// "UTC" / "+09" themselves).
    #[must_use]
    pub fn abbrev_at(&self, utc_micros: i64) -> Option<alloc::string::String> {
        match self {
            Self::Named(zone, _, f) => f(zone, utc_micros),
            _ => None,
        }
    }
}

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
    /// v7.37.15 (Phase E) — cached MVCC snapshot for REPEATABLE
    /// READ / SERIALIZABLE. Captured at `exec_begin` time when the
    /// session's `current_isolation_level` is RR/SER; read paths
    /// inside the TX use this snapshot rather than calling
    /// `Engine::current_snapshot` per statement, so a row that
    /// becomes visible mid-tx (because another writer committed)
    /// is NOT exposed to this tx — preserving RR's invariant.
    ///
    /// `None` for READ COMMITTED (default): each statement gets a
    /// fresh snapshot via `current_snapshot()`.
    cached_snapshot: Option<spg_storage::snapshot::Snapshot>,
    /// v7.37.17 (Phase E2 — RC rebase) — tables this tx has run DML
    /// against. The per-statement rebase extracts/replays write-sets
    /// only for these (see `maybe_rc_rebase`).
    touched_tables: alloc::collections::BTreeSet<String>,
    /// v7.37.17 — the tx executed a statement whose effect on the
    /// shadow catalog can't be expressed as a versioned row write-set
    /// (DDL, COPY, anything unclassified). The rebase would lose it,
    /// so the tx degrades to its frozen BEGIN-time view (SI) for the
    /// rest of its life — the pre-E2 behaviour, honestly kept.
    rebase_poisoned: bool,
    /// v7.37.17 — statements successfully run inside this tx. The
    /// first statement sees the BEGIN-time clone unchanged (it IS the
    /// latest base at that point); rebasing starts from the second.
    stmts_run: u32,
    /// v7.37.17 (Phase E4 fix) — (old RowId → new RowId) pairs recorded
    /// by every in-place UPDATE this tx ran, keyed by table (RowIds are
    /// per-relation). An UPDATE's write-set is tombstone(old) +
    /// insert(new); when a rebase skips a CONFLICTING tombstone (the
    /// row was updated/deleted by a concurrently-committed tx), the
    /// paired insert must be dropped too — otherwise the row
    /// DUPLICATES (caught by the E4 isolation matrix).
    update_pairs:
        alloc::collections::BTreeMap<String, Vec<(spg_storage::row_header::RowId, spg_storage::row_header::RowId)>>,
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

/// v7.39 (parallel-agg P0) — host-injected parallel executor. The
/// engine is `no_std` and cannot spawn threads; like `ClockFn` /
/// `RandomFn`, the std-side host (spg-server / embedded-tokio)
/// injects an implementation at startup. `None` (the default, and
/// the only option in pure-`no_std` embeddings) keeps every code
/// path single-threaded and byte-identical to pre-P0 behaviour.
///
/// The callback returns `Box<dyn Any + Send>` so one trait serves
/// any shard-result type; call sites downcast what they produced.
pub trait ParallelRunner: Send + Sync {
    /// Run `f(0) .. f(n-1)`, possibly concurrently; return the
    /// results in shard order. Every call completes before return.
    fn run_shards(
        &self,
        n: usize,
        f: &(dyn Fn(usize) -> alloc::boxed::Box<dyn core::any::Any + Send> + Sync),
    ) -> alloc::vec::Vec<alloc::boxed::Box<dyn core::any::Any + Send>>;
}

/// Engine slot for the injected runner — a newtype so the `Engine`
/// derive(Debug) keeps working over the non-Debug trait object.
#[derive(Clone, Default)]
pub struct ParallelRunnerSlot(pub(crate) Option<alloc::sync::Arc<dyn ParallelRunner>>);

impl core::fmt::Debug for ParallelRunnerSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if self.0.is_some() {
            "ParallelRunner(<injected>)"
        } else {
            "ParallelRunner(none)"
        })
    }
}

/// v7.39 (parallel-agg P0) — below this many input rows a query
/// never parallelises: thread spin-up beats the win on small scans.
pub(crate) const PARALLEL_MIN_ROWS: usize = 100_000;

/// v7.39 — diagnostic counter: how many aggregate scans took the
/// sharded path (read by benches to ground-truth activation).
pub static PARALLEL_AGG_FIRED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

// The engine carries several independent session/capture flags (dialect,
// FK-checks, meta-view materialisation, redo capture); they're orthogonal
// switches, not a state enum begging to be modelled.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct Engine {
    /// v7.39 (parallel-agg P0) — see [`ParallelRunner`].
    pub(crate) parallel_runner: ParallelRunnerSlot,
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
    /// v7.37.15 (Phase C) — versions allocated by in-flight
    /// writers. Snapshot construction folds this into the
    /// `Snapshot.in_progress` set so concurrent readers (or readers
    /// inside an older snapshot's REPEATABLE READ) don't see
    /// uncommitted writes.
    ///
    /// SPG's single-writer invariant means at most one writer
    /// version sits here at any moment (the one currently
    /// executing inside the engine write lock). The set survives
    /// engine clones because tx-commit removes versions before the
    /// `Engine::snapshot_data` returns, so a snapshot taken after
    /// commit observes an empty set.
    ///
    /// `BTreeSet` (not Vec) so iteration is sorted — Snapshot
    /// constructor expects the input sorted for binary-search
    /// `contains` correctness.
    active_writer_versions: BTreeSet<u64>,
    /// v7.37.15 (Phase C.2) — writer versions that ABORTED (rolled
    /// back). The engine-side visibility oracle ([`Self::xact_status`])
    /// consults this to report `Aborted` for a version that left the
    /// in-flight set via rollback rather than commit — the third state
    /// the abort-aware [`spg_storage::snapshot::Snapshot::visible_with_status`]
    /// needs once Phase C.3's in-place writes leave aborted stamps in
    /// place. Pruned below `oldest_active` by vacuum (Phase D); until
    /// then it grows only with rolled-back transactions (a never-die
    /// follow-up, not a commit-path leak).
    aborted_versions: BTreeSet<u64>,
    /// v7.37.15 (Phase C.4) — row-level lock table keyed on stable
    /// `(RelId, RowId)`. The in-place write path (C.3) acquires a
    /// tuple lock before stamping xmax; `exec_commit` / `exec_rollback`
    /// release the whole transaction's locks at end. No writer acquires
    /// yet at this commit — the field + delegating methods are the
    /// plumbing the write path consumes next. Rides on `Engine` like
    /// `active_writer_versions`; the sharded lock-free manager is C.5.
    locks: crate::locks::LockTable,
    /// v7.37.15 (Phase C.3) — kill switch for the in-place MVCC write
    /// path. `false` (default) = legacy physical semantics (DELETE
    /// physically removes the row, UPDATE replaces in place). `true` =
    /// the C.3 write path (DELETE tombstones via `mark_row_deleted`,
    /// UPDATE tombstones the old version + appends the new one, both
    /// keeping dead versions physically present for the now-uniformly-
    /// gated readers until vacuum reclaims them). `no_std` engine can't
    /// read env; the host (spg-server / spg-embedded) reads
    /// `SPG_MVCC_INPLACE` and calls [`Self::set_mvcc_inplace`]. Off
    /// until the write path + PG18 differential tests are proven.
    mvcc_inplace: bool,
    /// v7.37.16 — threshold-triggered synchronous vacuum at DML statement
    /// exit (autovacuum-lite; see .claude/state/autovacuum-design.md).
    /// Default ON; hosts may disable via `SPG_AUTOVACUUM=0`.
    autovacuum: bool,
    /// v7.37.15 (Phase C) — TxId → writer version registry. When
    /// `exec_begin` opens an explicit transaction it allocates a
    /// fresh writer version (via [`Self::begin_writer_version`])
    /// and stashes the mapping here so the matching `exec_commit`
    /// / `exec_rollback` can call
    /// [`Self::commit_writer_version`] on the right entry. Empty
    /// when no explicit transactions are open.
    tx_writer_versions: BTreeMap<TxId, u64>,
    /// v7.37.15 (Epic W slice 2) — the current statement's autocommit
    /// writer version, memoized. In autocommit
    /// [`Self::writer_version_for_current_stmt`] mints a fresh version
    /// via `next_version()` (a `fetch_add`), so calling it a second
    /// time — e.g. when the redo drain post-stamps `RowChange`s —
    /// would allocate a *different* number than the writes actually
    /// used. Memoizing the first allocation for the duration of one
    /// statement makes the drain stamp read back the exact version the
    /// rows were written with, without advancing the counter twice.
    /// `None` outside a statement / before the first fetch; saved and
    /// reset per `execute_in_with_cancel` so it never leaks across
    /// statements. Explicit transactions bypass this (their version is
    /// the deterministic `tx_writer_versions` entry).
    stmt_writer_version: Option<u64>,
    /// v7.22 (round-13 T3) — session string-literal dialect. `false`
    /// (default) = PG semantics (backslash literal, `''` escape);
    /// `true` = MySQL semantics (`\'` etc.). Flipped by the
    /// deterministic session signals each dump emits: `SET sql_mode`
    /// (only MySQL clients/dumps send it) turns it on,
    /// `SET standard_conforming_strings = on` (every pg_dump
    /// preamble) turns it off. The plan cache is cleared on every
    /// flip — the same SQL text lexes differently per dialect.
    backslash_escapes: bool,
    /// v7.37.17 — name of the sequence most recently advanced by
    /// nextval() in this Engine (session). Backs PG's lastval().
    /// None until the first nextval; PG errors in that state.
    last_sequence_used: Option<String>,
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
    /// v7.38 (read01 P3.12) — cumulative row-write counters feeding
    /// `pg_stat_database` (database-wide `tup_inserted` / `tup_updated` /
    /// `tup_deleted`). Bumped by the affected-row count of each successful
    /// INSERT / UPDATE / DELETE statement. Per-Engine (so tests stay
    /// isolated); on the server's shared engine they read as the
    /// since-start database totals PG reports.
    /// v7.39 (pg_stat knife A) — committed / rolled-back transaction
    /// counters for pg_stat_database. Atomics so the read-only
    /// autocommit path (&self) can count its implicit commit, matching
    /// PG (every successful statement outside a tx block is one
    /// xact_commit — SELECTs included).
    pub(crate) xact_commit: core::sync::atomic::AtomicU64,
    pub(crate) xact_rollback: core::sync::atomic::AtomicU64,
    /// v7.39 (pg_stat knife A) — host-injected live backend count for
    /// pg_stat_database.numbackends (ClockFn-style fn slot; the server
    /// wires its connection registry, embedded stays None -> 1).
    pub(crate) backend_count_fn: Option<BackendCountFn>,
    pub(crate) backend_pid_fn: Option<BackendPidFn>,
    /// v7.39 (tz epic) — injected IANA timezone lookups; None on a
    /// host without zoneinfo (named zones then fail to SET, honestly).
    pub(crate) tz_offset_fn: Option<TzOffsetFn>,
    pub(crate) tz_localize_fn: Option<TzLocalizeFn>,
    pub(crate) tz_canon_fn: Option<TzCanonFn>,
    pub(crate) tz_abbrev_fn: Option<TzAbbrevFn>,
    pub(crate) stat_tup_inserted: u64,
    pub(crate) stat_tup_updated: u64,
    pub(crate) stat_tup_deleted: u64,
    /// v7.38 (read01 P3.19) — `SET LOCAL` undo log for the current
    /// transaction. Each entry is `(param_name, prior_value)` captured
    /// just before a `SET LOCAL` overwrote it (`None` = the param had no
    /// session value, so restoring means removing it). Replayed in
    /// reverse at COMMIT / ROLLBACK to revert transaction-local settings;
    /// `savepoint_guc_marks` records the stack depth at each open
    /// savepoint so `ROLLBACK TO` can unwind just the later ones.
    pub(crate) local_guc_saves: Vec<(String, Option<String>)>,
    /// v7.39 (GUC knife 3) — parsed DateStyle / IntervalStyle /
    /// extra_float_digits, kept in lockstep with `session_params` so
    /// renderers don't re-parse GUC text per cell.
    pub(crate) render_style: crate::eval::RenderStyle,
    pub(crate) savepoint_guc_marks: Vec<(String, usize)>,
    /// v7.38 (read01 P3.26) — set when a statement fails inside an explicit
    /// transaction; while true every statement except COMMIT / ROLLBACK /
    /// ROLLBACK TO SAVEPOINT is rejected with [`EngineError::InFailedTransaction`],
    /// matching PG's aborted-transaction semantics. Cleared when the tx ends
    /// or a ROLLBACK TO SAVEPOINT recovers it.
    pub(crate) tx_aborted: bool,
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
    /// v7.38 元机制 D — frozen snapshot of `SPG_TEST_*` env vars. Read
    /// once at construction (`with_env_cfg`) and queried on hot paths
    /// via `engine.env_cfg().<field>`. Production builds keep this at
    /// `EnvConfig::default()`, so the optimiser can const-fold every
    /// `if env_cfg.<field>` gate. See `testkit::env_config` + the
    /// `xtests/sigil/test-mode-gucs.md` index.
    env_cfg: testkit::EnvConfig,
    /// v7.38 P0 元机制 A — per-engine `injection_points` attach
    /// table. Only exists when the crate is built with the
    /// `injection-points` feature; release builds carry no field.
    /// Pushed onto the thread-local stack by
    /// `enter_injection_scope()` so the `injection_point!()` macro
    /// can find it from anywhere in the executor without rewiring
    /// every signature. See
    /// `crates/spg-engine/src/testkit/injection.rs`.
    #[cfg(feature = "injection-points")]
    injection_store: alloc::sync::Arc<crate::testkit::injection::InjectionStore>,
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
    /// v7.38 轴 4 — currently-selected SQL isolation level. Set by
    /// `SET TRANSACTION ISOLATION LEVEL …`; read by
    /// `SHOW transaction_isolation`. v7.37.8 implements the
    /// SQL surface; actual semantic differentiation (REPEATABLE READ
    /// snapshot / SERIALIZABLE SSI) lands in a separate train.
    pub(crate) current_isolation_level: spg_sql::ast::IsolationLevel,
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
    /// v7.37.14 (B6.3) — PG-style wait-event categorisation
    /// ("Lock", "LWLock", "IPC", "IO", "Timeout", "Client",
    /// "BufferPin", "Extension", ""). Empty string means idle.
    /// Pair with `wait_event` to identify "what specifically is
    /// the backend waiting on" the same way PG does.
    pub wait_event_type: String,
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
            parallel_runner: ParallelRunnerSlot::default(),
            tx_catalogs: BTreeMap::new(),
            current_tx: None,
            backslash_escapes: false,
            last_sequence_used: None,
            next_tx_id: 1,
            active_writer_versions: BTreeSet::new(),
            aborted_versions: BTreeSet::new(),
            locks: crate::locks::LockTable::new(),
            mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
            autovacuum: true,
            tx_writer_versions: BTreeMap::new(),
            stmt_writer_version: None,
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
            xact_commit: core::sync::atomic::AtomicU64::new(0),
            xact_rollback: core::sync::atomic::AtomicU64::new(0),
            backend_count_fn: None,
            backend_pid_fn: None,
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_canon_fn: None,
            tz_abbrev_fn: None,
            stat_tup_inserted: 0,
            stat_tup_updated: 0,
            stat_tup_deleted: 0,
            local_guc_saves: Vec::new(),
            render_style: crate::eval::RenderStyle::default(),
            savepoint_guc_marks: Vec::new(),
            tx_aborted: false,
            trigger_recursion_depth: 0,
            foreign_key_checks: true,
            meta_views_materialised: false,
            pending_foreign_keys: Vec::new(),
            env_cfg: testkit::EnvConfig::default(),
            #[cfg(feature = "injection-points")]
            injection_store: alloc::sync::Arc::new(
                crate::testkit::injection::InjectionStore::default(),
            ),
            redo_capture: false,
            current_isolation_level: spg_sql::ast::IsolationLevel::ReadCommitted,
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

    /// v7.37.15 (Phase B / C / E) — current per-row visibility
    /// snapshot for in-engine scans. Captures the live writer-
    /// version cursor + active-writer set; readers built from this
    /// Snapshot see committed state through the moment of capture
    /// and DO NOT observe uncommitted writes still inside
    /// `active_writer_versions`.
    ///
    /// Phase E: if there's an explicit transaction in flight under
    /// REPEATABLE READ or SERIALIZABLE isolation, returns the
    /// snapshot the tx cached at BEGIN time — every statement in
    /// the tx sees the same coherent prior-committed view. READ
    /// COMMITTED (the default) returns a fresh snapshot per call,
    /// matching PG's per-statement visibility semantics.
    ///
    /// `oldest_active = version` when no writer is in flight (== no
    /// dead row could still be observed); else == min of active
    /// versions (vacuum-floor).
    #[must_use]
    pub fn current_snapshot(&self) -> spg_storage::snapshot::Snapshot {
        // Phase E — if we're inside a RR/SER tx, return its
        // cached snapshot so the whole tx sees one frozen view.
        if let Some(tx_id) = self.current_tx
            && let Some(state) = self.tx_catalogs.get(&tx_id)
            && let Some(s) = state.cached_snapshot.as_ref()
        {
            return s.clone();
        }
        // v7.37.15 (Phase C.3, step 1) — carry the current tx's writer
        // version as the snapshot's `tx_id` so the visibility gate's
        // self-write branch (`visible` step 1) recognises rows this
        // transaction stamped (`xmin == v`, with `v` in
        // `active_writer_versions`). Without this the tx's own
        // uncommitted rows fall to the in-progress step and become
        // invisible to itself on the gated read paths. Autocommit reads
        // (no `tx_writer_versions` entry) keep `tx_id = 0`.
        let reader_tx_id = self
            .current_tx
            .and_then(|t| self.tx_writer_versions.get(&t).copied())
            .unwrap_or(0);
        let version = spg_storage::row_header::current_version();
        if self.active_writer_versions.is_empty() {
            // Hot path: no writer in flight. Snapshot::unbounded()
            // would also work, but pinning to the live cursor
            // means the snapshot's oldest_active is accurate
            // (= version) so subsequent vacuum can advance.
            return spg_storage::snapshot::Snapshot::new(
                version,
                spg_storage::snapshot::InProgressSet::empty(),
                version,
                reader_tx_id,
            );
        }
        let sorted: alloc::vec::Vec<u64> = self.active_writer_versions.iter().copied().collect();
        let oldest = *sorted.first().unwrap_or(&version);
        spg_storage::snapshot::Snapshot::new(
            version,
            spg_storage::snapshot::InProgressSet::from_sorted(sorted),
            oldest,
            reader_tx_id,
        )
    }

    /// v7.37.15 (Phase C) — allocate the next writer version AND
    /// add it to the in-flight set so concurrent snapshots hide
    /// the resulting writes until [`Self::commit_writer_version`]
    /// removes the entry. Returns the allocated version so the
    /// writer can stamp it on `xmin` / `xmax`.
    pub fn begin_writer_version(&mut self) -> u64 {
        let v = spg_storage::row_header::next_version();
        self.active_writer_versions.insert(v);
        v
    }

    /// v7.37.15 (Phase C) — mark a previously-allocated writer
    /// version as committed. Subsequent snapshots stop including
    /// it in `in_progress`, so the writes the version stamped
    /// become visible to new readers.
    ///
    /// No-op if the version was never allocated; matches PG's
    /// idempotent `TransactionIdCommitTree` semantics.
    pub fn commit_writer_version(&mut self, v: u64) {
        self.active_writer_versions.remove(&v);
    }

    /// v7.37.15 (Phase C.2) — mark a previously-allocated writer
    /// version as ABORTED (rolled back). Removes it from the in-flight
    /// set and records it in `aborted_versions` so the visibility
    /// oracle ([`Self::xact_status`]) reports `Aborted` rather than
    /// silently treating it as committed once it leaves the in-flight
    /// set. Phase C.3's in-place write path relies on this: a
    /// rolled-back version's xmin/xmax stamps stay physically present
    /// until vacuum reclaims them, and readers must NOT see them.
    ///
    /// Idempotent; a no-op if the version was never allocated.
    pub fn abort_writer_version(&mut self, v: u64) {
        self.active_writer_versions.remove(&v);
        self.aborted_versions.insert(v);
    }

    /// v7.37.15 (Phase C.2) — the visibility oracle's terminal-status
    /// lookup for one version. In-flight if still allocated, Aborted
    /// if it rolled back, otherwise Committed (the default for a
    /// version that left the in-flight set the normal way, and for
    /// every frozen / pruned old version the engine no longer tracks).
    ///
    /// `aborted_versions` is bounded by pruning below `oldest_active`
    /// during vacuum (Phase D): once no live snapshot can still see an
    /// aborted version's stamps, its entry is dropped. Until Phase D
    /// lands the set only grows with rolled-back transactions — noted
    /// as a never-die follow-up, not a steady-state leak on the
    /// commit path.
    #[must_use]
    pub fn xact_status(&self, v: u64) -> spg_storage::snapshot::XactStatus {
        use spg_storage::snapshot::XactStatus;
        if self.active_writer_versions.contains(&v) {
            XactStatus::InProgress
        } else if self.aborted_versions.contains(&v) {
            XactStatus::Aborted
        } else {
            XactStatus::Committed
        }
    }

    /// v7.37.15 (Phase C.4) — acquire a tuple lock on a stable
    /// `(RelId, RowId)` for writer `version`. The in-place write path
    /// (C.3) calls this before stamping xmax; `SELECT ... FOR UPDATE`
    /// wires here via the parser's lock-strength clause (C.4). Returns
    /// the [`LockOutcome`](crate::locks::LockOutcome) the caller acts on
    /// (grant / park / skip / fail / deadlock-abort).
    pub fn acquire_row_lock(
        &mut self,
        rel: spg_storage::row_header::RelId,
        row: spg_storage::row_header::RowId,
        mode: crate::locks::LockMode,
        version: u64,
        policy: crate::locks::WaitPolicy,
    ) -> crate::locks::LockOutcome {
        self.locks.acquire(rel, row, mode, version, policy)
    }

    /// v7.37.15 (Phase C.4) — release every lock + wait held by
    /// `version` at transaction end. Called from `exec_commit` /
    /// `exec_rollback` alongside the writer-version bookkeeping.
    pub fn release_tx_locks(&mut self, version: u64) {
        self.locks.release_all(version);
    }

    /// v7.37.15 (Phase C.4) — number of rows currently locked, for the
    /// `pg_locks` enumeration and tests.
    #[must_use]
    pub fn locked_row_count(&self) -> usize {
        self.locks.locked_row_count()
    }

    /// v7.37.15 (Phase C.3) — is the in-place MVCC write path enabled?
    /// `false` (default) keeps legacy physical DELETE/UPDATE. The C.3
    /// writers consult this to choose tombstone-vs-physical.
    #[must_use]
    pub fn mvcc_inplace(&self) -> bool {
        self.mvcc_inplace
    }

    /// v7.37.15 (Phase C.3) — enable/disable the in-place MVCC write
    /// path. Called by the host after reading `SPG_MVCC_INPLACE` (the
    /// `no_std` engine can't read the environment itself). Off until
    /// the write path is proven against PG18 differential tests.
    pub fn set_mvcc_inplace(&mut self, on: bool) {
        self.mvcc_inplace = on;
    }

    /// v7.39 (parallel-agg P0) — inject the host's parallel executor
    /// (see [`ParallelRunner`]). Called once at host startup; the
    /// engine stays single-threaded without it.
    /// v7.39 (pg_stat knife A) — inject the host's live backend count.
    pub fn set_backend_count_fn(&mut self, f: BackendCountFn) {
        self.backend_count_fn = Some(f);
    }

    /// v7.39 (read01 pgstatfuncs.c) — inject the host's calling-connection
    /// identity for pg_backend_pid().
    pub fn set_backend_pid_fn(&mut self, f: BackendPidFn) {
        self.backend_pid_fn = Some(f);
    }

    /// v7.39 (tz epic) — inject the host's IANA timezone lookups
    /// (spg-tzif's fn family on std hosts).
    pub fn set_tz_fns(
        &mut self,
        offset: TzOffsetFn,
        localize: TzLocalizeFn,
        canon: TzCanonFn,
        abbrev: TzAbbrevFn,
    ) {
        self.tz_offset_fn = Some(offset);
        self.tz_localize_fn = Some(localize);
        self.tz_canon_fn = Some(canon);
        self.tz_abbrev_fn = Some(abbrev);
    }

    pub fn set_parallel_runner(&mut self, runner: alloc::sync::Arc<dyn ParallelRunner>) {
        self.parallel_runner = ParallelRunnerSlot(Some(runner));
    }

    /// v7.37.15 (Phase C) — allocate a fresh version number for
    /// the next write. Always strictly monotonic + process-wide
    /// shared so concurrent engines on the same process agree on
    /// "tx 17 commits before tx 18". Phase C writer paths call
    /// this once per INSERT / UPDATE / DELETE statement to obtain
    /// the version they'll stamp on the new row's `xmin` (or the
    /// existing row's `xmax`).
    ///
    /// Returns [`XMIN_FROZEN`] when MVCC stamping is intentionally
    /// off (legacy `in_memory` flow / WAL replay): the writer
    /// then takes the legacy frozen-insert short-circuit path
    /// inside `Table::insert_with_xmin`.
    #[must_use]
    pub fn next_writer_version(&self) -> u64 {
        spg_storage::row_header::next_version()
    }

    /// v7.37.15 (Phase C) — version a writer should stamp on
    /// rows produced by the current statement. Inside an explicit
    /// transaction the version is the tx's pre-allocated one (so
    /// every statement in the tx commits atomically at COMMIT);
    /// in autocommit it allocates a fresh version per statement.
    ///
    /// This is the canonical helper engine writers should call
    /// — using it instead of `next_writer_version` ensures
    /// explicit-tx semantics where every row produced by the tx
    /// shares one xmin and concurrent readers don't see partial
    /// state until COMMIT.
    ///
    /// v7.37.15 (Epic W slice 2) — takes `&mut self` so the autocommit
    /// branch can **memoize** its freshly-minted version in
    /// `stmt_writer_version`. `next_writer_version()` is a `fetch_add`,
    /// so without memoization a second call within one statement (the
    /// redo drain post-stamps the captured `RowChange`s) would allocate
    /// a *different* version than the writes used. Memoizing makes the
    /// value stable for the statement's lifetime; it is reset per
    /// `execute_in_with_cancel`, so the counter still advances exactly
    /// once per autocommit statement — identical to before.
    pub fn writer_version_for_current_stmt(&mut self) -> u64 {
        if let Some(tx_id) = self.current_tx
            && let Some(&v) = self.tx_writer_versions.get(&tx_id)
        {
            return v;
        }
        // Autocommit shape: fresh version, immediately "committed"
        // (no entry in active_writer_versions, so subsequent
        // readers see the row). Memoized for the statement so the
        // redo drain reads back the same version the writes used.
        if let Some(v) = self.stmt_writer_version {
            return v;
        }
        let v = self.next_writer_version();
        self.stmt_writer_version = Some(v);
        v
    }

    /// Construct an engine restored from a previously-snapshotted catalog
    /// (see `snapshot()`).
    pub fn restore(catalog: Catalog) -> Self {
        Self {
            catalog,
            parallel_runner: ParallelRunnerSlot::default(),
            tx_catalogs: BTreeMap::new(),
            current_tx: None,
            backslash_escapes: false,
            last_sequence_used: None,
            next_tx_id: 1,
            active_writer_versions: BTreeSet::new(),
            aborted_versions: BTreeSet::new(),
            locks: crate::locks::LockTable::new(),
            mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
            autovacuum: true,
            tx_writer_versions: BTreeMap::new(),
            stmt_writer_version: None,
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
            xact_commit: core::sync::atomic::AtomicU64::new(0),
            xact_rollback: core::sync::atomic::AtomicU64::new(0),
            backend_count_fn: None,
            backend_pid_fn: None,
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_canon_fn: None,
            tz_abbrev_fn: None,
            stat_tup_inserted: 0,
            stat_tup_updated: 0,
            stat_tup_deleted: 0,
            local_guc_saves: Vec::new(),
            render_style: crate::eval::RenderStyle::default(),
            savepoint_guc_marks: Vec::new(),
            tx_aborted: false,
            trigger_recursion_depth: 0,
            foreign_key_checks: true,
            meta_views_materialised: false,
            pending_foreign_keys: Vec::new(),
            env_cfg: testkit::EnvConfig::default(),
            #[cfg(feature = "injection-points")]
            injection_store: alloc::sync::Arc::new(
                crate::testkit::injection::InjectionStore::default(),
            ),
            redo_capture: false,
            current_isolation_level: spg_sql::ast::IsolationLevel::ReadCommitted,
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
                    parallel_runner: ParallelRunnerSlot::default(),
                    tx_catalogs: BTreeMap::new(),
                    current_tx: None,
                    backslash_escapes: false,
                    last_sequence_used: None,
                    next_tx_id: 1,
                    active_writer_versions: BTreeSet::new(),
                    aborted_versions: BTreeSet::new(),
                    locks: crate::locks::LockTable::new(),
                    mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
            autovacuum: true,
                    tx_writer_versions: BTreeMap::new(),
                    stmt_writer_version: None,
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
                    xact_commit: core::sync::atomic::AtomicU64::new(0),
            xact_rollback: core::sync::atomic::AtomicU64::new(0),
            backend_count_fn: None,
            backend_pid_fn: None,
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_canon_fn: None,
            tz_abbrev_fn: None,
            stat_tup_inserted: 0,
                    stat_tup_updated: 0,
                    stat_tup_deleted: 0,
                    local_guc_saves: Vec::new(),
            render_style: crate::eval::RenderStyle::default(),
                    savepoint_guc_marks: Vec::new(),
                    tx_aborted: false,
                    trigger_recursion_depth: 0,
                    foreign_key_checks: true,
                    meta_views_materialised: false,
                    pending_foreign_keys: Vec::new(),
                    env_cfg: testkit::EnvConfig::default(),
                    #[cfg(feature = "injection-points")]
                    injection_store: alloc::sync::Arc::new(
                        crate::testkit::injection::InjectionStore::default(),
                    ),
                    redo_capture: false,
                    current_isolation_level: spg_sql::ast::IsolationLevel::ReadCommitted,
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

    /// v7.38 元机制 D — install a frozen [`testkit::EnvConfig`] snapshot.
    ///
    /// Hosts (spg-server, spg-embedded, tests) call this once at engine
    /// init with either `EnvConfig::from_env()` (production-with-test-vars)
    /// or `EnvConfig::builder()....build()` (programmatic). After
    /// construction the engine never reads env vars; all test-mode
    /// behaviour flows through `self.env_cfg()`.
    #[must_use]
    pub fn with_env_cfg(mut self, env_cfg: testkit::EnvConfig) -> Self {
        self.env_cfg = env_cfg;
        self
    }

    /// v7.38 元机制 D — frozen test-mode GUC snapshot. Hot paths gate
    /// nondeterministic surfaces on fields of this struct; production
    /// default keeps every field at `false / None / Auto` so the
    /// optimiser can const-fold the gate.
    pub fn env_cfg(&self) -> &testkit::EnvConfig {
        &self.env_cfg
    }

    /// v7.38 元机制 D acceptor — single seed source for every
    /// nondeterministic engine subsystem (hash builders, randomised
    /// tie-breakers, …). Honour `SPG_TEST_RANDOM_SEED=N` when set;
    /// otherwise derive from the engine's wall clock (production) or
    /// fall back to a fixed sentinel when the host hasn't installed
    /// a clock. Two engines built with the same builder seed return
    /// byte-equal output for the same query.
    /// See `xtests/sigil/test-mode-gucs.md`.
    pub fn rng_seed(&self) -> u64 {
        if let Some(seed) = self.env_cfg.random_seed {
            return seed;
        }
        match self.clock {
            Some(f) => f() as u64,
            // Production engines without a clock installed get a fixed
            // non-zero sentinel; same shape as PG's `random()` start
            // state under a `setseed(0)`.
            None => 0xBAD_5EED_DEAD_BEEF,
        }
    }

    /// v7.38 P0 元机制 A — push this engine's `InjectionStore` onto
    /// the thread-local stack so any `injection_point!()` reached
    /// during the returned guard's lifetime resolves against this
    /// engine. Mirrors PG's per-backend injection table.
    ///
    /// Returns a no-op guard when the `injection-points` feature is
    /// off so call sites don't need `#[cfg]`.
    pub fn enter_injection_scope(&self) -> crate::testkit::injection::InjectionGuard {
        #[cfg(feature = "injection-points")]
        {
            crate::testkit::injection::enter_scope(&self.injection_store)
        }
        #[cfg(not(feature = "injection-points"))]
        {
            crate::testkit::injection::new_guard()
        }
    }

    /// v7.38 P0 元机制 A — expose the per-engine store so tests can
    /// query notice counts / detach actions without parsing SQL
    /// output. Only present when the feature is on.
    #[cfg(feature = "injection-points")]
    pub fn injection_store(&self) -> alloc::sync::Arc<crate::testkit::injection::InjectionStore> {
        self.injection_store.clone()
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

    /// v7.37 C.5 (A.2) — per-connection in-transaction test. A given
    /// connection is "in a transaction" iff its own `tx_id` has an open
    /// shadow slot. Unlike [`in_transaction`] (which is true if *any* tx is
    /// open), this lets concurrent connections each carry their own explicit
    /// transaction without colliding on the global slot. `IMPLICIT_TX` never
    /// has a persistent slot (autocommit reads/writes the main catalog), so
    /// this is false for the autocommit id.
    pub fn is_tx_open(&self, tx_id: TxId) -> bool {
        self.tx_catalogs.contains_key(&tx_id)
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

    /// v7.37.8 — read accessor for tests / observability. The
    /// embedding layer flips this on once per `open_path` (after
    /// replay completes) when `SPG_WAL_ROW_REDO` is enabled (now
    /// default in v7.37.8). A consumer that wants to verify the
    /// post-upgrade contract ("writes go to V5 ROW_REDO by default")
    /// reads this through `Database::engine_redo_capture()` instead
    /// of inspecting WAL bytes (which the auto-checkpoint truncates
    /// on `Drop`).
    pub fn redo_capture_enabled(&self) -> bool {
        self.redo_capture
    }

    /// v7.38 轴 4 — currently-selected SQL isolation level. Default
    /// `ReadCommitted` after construction; updated by
    /// `SET TRANSACTION ISOLATION LEVEL …`. Read by
    /// `SHOW transaction_isolation` and any future MVCC/SSI gate.
    pub fn current_isolation_level(&self) -> spg_sql::ast::IsolationLevel {
        self.current_isolation_level
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
