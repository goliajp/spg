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

mod acl;
pub mod aggregate;
pub(crate) mod amcheck;
mod baregroup;
pub mod brin;
mod bytebudget;
mod cancel;
mod clock;
mod collate;
mod collate_derive;
mod collation_catalog;
mod constfold;
mod constraints;
mod conversions;
pub mod copy;
mod cursor;
mod ddl;
pub mod describe;
mod distinct;
mod dml;
mod dump;
mod envelope;
pub mod eval;
mod execute;
mod explain;
mod expr_analysis;
mod expr_index;
pub(crate) mod extsort;
pub mod fts;
mod fts_es;
mod fts_stop;
mod guc_catalog;
mod immutable_fn;
mod index_access;
mod join;
mod join_using;
mod joinfold;
pub mod json;
pub mod largeobject;
mod limit_expr;
pub mod locks;
mod maintenance;
pub mod memoize;
mod notify;
mod numeric;
mod opclass;
mod orderby;
mod partition;
pub(crate) mod partition_walks;
pub mod plan_cache;
mod plpgsql;
pub mod publications;
mod qualorder;
pub mod query_stats;
mod readonly;
pub mod reorder;
mod rls;
mod rules;
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
pub mod tempstore;
pub mod testkit;
mod transaction;
pub(crate) use transaction::{TxStmtClass, classify_stmt_for_tx};
pub mod triggers;
pub mod users;
mod window;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};
pub use cancel::{CancelToken, MonotonicNowFn};
pub use execute::{RowCells, StreamItem};

use bytebudget::*;
pub(crate) use clock::{rewrite_clock_calls, value_to_literal};
use constraints::*;
pub use constraints::{UNIQ_FOLD_CHOSEN, UNIQ_PROBE_CALLS, UNIQ_PROBE_LOCATORS};
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
    order_by_value_cmp_in, render_histogram_bounds, resolve_order_by_position, sort_by_keys,
    sort_values_for_histogram, topk_trim, value_cmp, value_to_f64,
};
pub use select::{DISTINCT_DUP_DROPPED, PROJ_DIRECT_FIRE, PROJ_ROW_BUILT, SCAN_PATH_ENTERED};
pub(crate) use select::{build_projection, infer_column_types, value_to_order_key};
pub use sequence::MUTATING_CALL_NEEDLES;
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
    /// v7.39 (round 299, E3 Phase 2) — a row lock is held by another
    /// transaction and the policy is `Wait`.
    ///
    /// Its own variant, not an `Unsupported` string: the SERVER has to
    /// recognise it to retry, and it cannot block inside the engine
    /// write lock — doing so would stop the whole server, including the
    /// transaction whose commit would release the lock.
    LockWouldBlock,
    /// v7.39 (round 299) — granting the wait would close a wait-for
    /// cycle. PG's 40P01.
    LockDeadlock,
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
    /// v7.39 (round 318, V51) — MySQL `KILL <id>` naming an id no live
    /// connection carries. MariaDB 11: `ERROR 1094 (HY000) Unknown thread
    /// id: N`.
    UnknownThreadId(u32),
    /// v7.39 (round 318, V51) — MySQL `KILL <own id>`. The connection
    /// really is killed; MariaDB reports it to the victim as
    /// `ERROR 1927 (70100) Connection was killed` and closes.
    ConnectionKilled,
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
            Self::LockWouldBlock => f.write_str("row is locked by another transaction"),
            Self::LockDeadlock => f.write_str("deadlock detected"),
            Self::InFailedTransaction => f.write_str(
                "current transaction is aborted, commands ignored until end of transaction block",
            ),
            Self::CardinalityViolation => {
                f.write_str("more than one row returned by a subquery used as an expression")
            }
            Self::SerializationFailure(detail) => {
                // v7.39 (round 552) — PG has TWO wordings under 40001 and
                // they mean different things: "concurrent update" for a
                // write-write conflict, "read/write dependencies among
                // transactions" for the antidependency a SERIALIZABLE
                // transaction hits. A detail that already carries PG's
                // own sentence is passed through rather than nested
                // inside the other one.
                if detail.starts_with("could not serialize access") {
                    f.write_str(detail)
                } else {
                    write!(
                        f,
                        "could not serialize access due to concurrent update: {detail}"
                    )
                }
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
            Self::UnknownThreadId(id) => write!(f, "Unknown thread id: {id}"),
            Self::ConnectionKilled => f.write_str("Connection was killed"),
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

/// Backing store for `SPG_TEST_FIXED_CLOCK_MICROS` (test-mode GUC).
/// Only ever written when that GUC is set; production engines never
/// touch it.
static FIXED_CLOCK_MICROS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

fn fixed_clock_from_env() -> i64 {
    FIXED_CLOCK_MICROS.load(core::sync::atomic::Ordering::Relaxed)
}

/// v7.39 (pg_stat knife A) — host-provided live connection count for
/// `pg_stat_database.numbackends`.
pub type BackendCountFn = fn() -> u32;

/// v7.39 (read01 pgstatfuncs.c) — host-provided identity of the CALLING
/// connection for `pg_backend_pid()` / the pg_stat_activity self-join.
/// The host reads a connection-thread-local set at session start; the
/// no_std engine just calls through. `None` (embedded) → pid 1.
pub type BackendPidFn = fn() -> u32;

/// v7.39 (round 476) — the WAL's current byte position, as a PG LSN.
///
/// `pg_current_wal_lsn()` answered the literal `0/0` forever, so every
/// monitor watching WAL progress or replication lag saw an instance that
/// had never written anything. SPG's WAL is a file and its length IS an
/// LSN in every sense a monitor uses one: monotonic, byte-denominated, and
/// comparable — `pg_wal_lsn_diff` over two samples gives real bytes.
///
/// `None` (embedded, or a server started without a WAL) keeps `0/0`, which
/// is the honest answer there: nothing is being written.
pub type WalLsnFn = fn() -> u64;

/// v7.39 (round 318, V51) — host-provided connection control. `terminate`
/// false = cancel the target's running statement (PG `pg_cancel_backend`,
/// MySQL `KILL QUERY`); true = also close the connection (PG
/// `pg_terminate_backend`, MySQL `KILL CONNECTION`). Returns whether a
/// connection with that id exists — the engine has no registry of its own,
/// so the answer has to come from the host that accepted the sockets.
/// `None` (embedded, no connections) ⇒ nothing to signal.
pub type BackendSignalFn = fn(pid: u32, terminate: bool) -> bool;

pub use tempstore::{SpillStats, TempRun, TempRunFactory, TempStoreError};

/// v7.39 (tz epic) — host-injected IANA timezone lookups (the no_std
/// engine can't read the system zoneinfo directory; spg-tzif is the
/// std-side implementation). All instants are MICROSECONDS.
/// UTC offset (µs east) of a zone at a UTC instant; None = unknown zone.
/// v7.39 (round 534) — the compiled-in default PG18 reports for a
/// configuration parameter, for the wire's own SHOW shortcut.
///
/// The pgwire layer answers `SHOW <name>` from a small canned list
/// before the statement ever reaches the engine, so it needs the same
/// inventory the engine reads or the two disagree — which they did:
/// `SHOW fsync` over the wire returned an empty row.
#[must_use]
pub fn pg_guc_boot_value(name: &str) -> Option<&'static str> {
    crate::guc_catalog::guc_boot_value(name)
}

pub type TzOffsetFn = fn(&str, i64) -> Option<i64>;
/// Local wall-clock µs -> UTC µs with PG's DST disambiguation.
pub type TzLocalizeFn = fn(&str, i64) -> Option<i64>;
/// Canonical zone spelling ("asia/tokyo" -> "Asia/Tokyo").
pub type TzCanonFn = fn(&str) -> Option<alloc::string::String>;
/// Zone designation ("JST", "EDT") at a UTC instant.
pub type TzAbbrevFn = fn(&str, i64) -> Option<alloc::string::String>;
/// v7.39 (round 502) — every zone the host knows at a UTC instant, as
/// `(name, abbrev, utc_offset_secs, is_dst)`.
///
/// Backs `pg_timezone_names`. SPG resolved named zones correctly — round
/// 502 measured DST boundaries byte-identical to PG18 — but could not
/// LIST them, so a client populating a timezone picker got "relation
/// pg_timezone_names does not exist". The data was there, only
/// unlistable. No hook, or a host with no tzdata, yields an empty view
/// rather than an error: that is what such a host honestly has.
pub type TzAllFn =
    fn(i64) -> alloc::vec::Vec<(alloc::string::String, alloc::string::String, i64, bool)>;

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
    /// v7.37 (round 828) — the TX's shadow copy of the user store,
    /// following exactly the catalog's model one field up: created
    /// lazily by the first role DDL inside the TX (an ordinary TX
    /// never pays the clone), written through for the rest of the TX,
    /// installed over `Engine.users` at COMMIT, discarded on ROLLBACK.
    /// PG treats roles as ordinary catalog rows — `BEGIN; CREATE ROLE
    /// r; ROLLBACK` leaves no role — and SPG used to refuse the
    /// statement instead, which no drop-in client expects.
    ///
    /// Other sessions and the auth path keep reading the committed
    /// store, so an uncommitted role can neither log in nor be seen
    /// elsewhere — the isolation PG gives via its catalog MVCC.
    users: Option<crate::users::UserStore>,
    /// Per-TX savepoint stack. Each entry pairs the savepoint name with
    /// a clone of `catalog` (and of the role shadow, which subtransactions
    /// roll back too) at the moment `SAVEPOINT <name>` fired.
    /// `ROLLBACK TO <name>` restores from the entry and pops everything
    /// after it; `RELEASE <name>` discards the entry and everything
    /// after; COMMIT/ROLLBACK clears the whole stack.
    savepoints: Vec<(String, Catalog, Option<crate::users::UserStore>)>,
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
    /// v7.39 (round 552) — tables this tx has READ. PG's SIREAD locks,
    /// at table granularity: the coarse end of the same idea, and what
    /// PG itself falls back to when its per-tuple lock memory runs out.
    ///
    /// A SERIALIZABLE tx aborts at COMMIT if any table it read was
    /// written by a transaction that committed after its snapshot —
    /// the read/write antidependency SI cannot see. Coarse granularity
    /// means SPG aborts some transactions PG would let through; it
    /// never lets through one PG would abort.
    read_tables: alloc::collections::BTreeSet<String>,
    /// v7.39 (round 552) — was THIS transaction opened SERIALIZABLE?
    ///
    /// `Engine::current_isolation_level` is one field for the whole
    /// engine, not part of the per-session bag, so with two connections
    /// open one transaction's COMMIT resets it under the other's feet —
    /// the shared-engine leak rounds 279 and 283 chased through session
    /// state and advisory locks. The level a transaction runs at has to
    /// live on the transaction.
    serializable: bool,
    /// The engine's commit sequence when this tx began.
    begin_commit_seq: u64,
    /// v7.39 (round 494) — has anything asked for this shadow catalog
    /// MUTABLY since BEGIN?
    ///
    /// COMMIT installs the shadow over the committed catalog, so a
    /// transaction that changed nothing must install nothing — otherwise
    /// it reverts whatever other sessions committed while it was open.
    /// `touched_tables` cannot answer this: it records DML targets for the
    /// rebase, and a `SELECT lo_write(…)` classifies read-only while
    /// mutating (the large-object pins caught exactly that).
    ///
    /// Set in `active_catalog_mut`, the single place a `&mut Catalog` is
    /// handed out. A caller that takes the mutable handle without writing
    /// merely keeps the old install behaviour, so the flag errs toward
    /// installing.
    shadow_dirty: bool,
    /// v7.39 (round 298) — this transaction is in the aborted state.
    ///
    /// Per SLOT. It used to be one flag on the shared `Engine`, guarded
    /// by `in_transaction()` — the GLOBAL "is any transaction open"
    /// test. So an autocommit statement that failed while a DIFFERENT
    /// connection happened to hold a transaction set the flag, and
    /// every other connection was then refused with 25P02. Round 283
    /// fixed seven sites of this exact shape in the server; this one is
    /// in the engine and was missed.
    aborted: bool,
    /// v7.39 (round 288) — `SET CONSTRAINTS … {DEFERRED|IMMEDIATE}`
    /// override for this transaction. `None` = each constraint uses
    /// its own declared timing; `Some(true)` = every DEFERRABLE one is
    /// deferred; `Some(false)` = every one is immediate.
    constraints_deferred: Option<bool>,
    /// v7.39 (round 308) — per-constraint overrides from the NAMED form
    /// of `SET CONSTRAINTS`. Consulted before `constraints_deferred`, so
    /// `ALL DEFERRED` followed by `fk_a IMMEDIATE` leaves fk_a immediate
    /// and everything else deferred. An `ALL` form clears this map,
    /// which is what makes a later blanket setting win — PG resets the
    /// whole set the same way.
    constraints_deferred_by_name: BTreeMap<String, bool>,
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
    /// v7.39 (round 196) — the engine `commit_epoch` this tx last
    /// rebased against (BEGIN seeds it). When the epoch hasn't moved,
    /// no other path committed to the base catalog, so the
    /// per-statement RC rebase — whose write-set extraction is a full
    /// scan of every touched table — is skipped entirely. The r196
    /// wire panel traced tx_batch's 2.8× LOSS to exactly that scan
    /// running before EVERY in-tx statement (~200 µs/stmt on a
    /// 20k-row table, 58× the statement itself). Over-incrementing
    /// the epoch is safe (an extra rebase is only slower, never
    /// wrong); missing an increment would be a correctness bug, so
    /// the epoch bumps on every completed non-tx statement.
    rebased_at_epoch: u64,
    /// v7.37.17 (Phase E4 fix) — (old RowId → new RowId) pairs recorded
    /// by every in-place UPDATE this tx ran, keyed by table (RowIds are
    /// per-relation). An UPDATE's write-set is tombstone(old) +
    /// insert(new); when a rebase skips a CONFLICTING tombstone (the
    /// row was updated/deleted by a concurrently-committed tx), the
    /// paired insert must be dropped too — otherwise the row
    /// DUPLICATES (caught by the E4 isolation matrix).
    update_pairs: alloc::collections::BTreeMap<
        String,
        Vec<(
            spg_storage::row_header::RowId,
            spg_storage::row_header::RowId,
        )>,
    >,
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
/// v7.39 (round 279) — the per-CONNECTION state, parked while another
/// connection holds the engine.
///
/// The server runs ONE shared `Engine` behind a `RwLock`
/// (`ServerState.engine`, built once at startup), so everything the
/// engine called "session state" was in fact process-wide and leaked
/// between clients: two connections saw each other's prepared
/// statements, and one client's `SET sql_mode` re-dialected another's
/// string literals. PG scopes all of this per session.
///
/// Rather than thread a session handle through every call site, the
/// engine keeps the ACTIVE session's state in its own fields — so the
/// ~40 existing `self.session_params` / `self.backslash_escapes` uses
/// are untouched — and swaps the whole bag when the caller announces a
/// different session. Embedded hosts never announce one and stay on
/// session 0 forever, exactly as before.
/// v7.38.18 (C12) — one row of MySQL's diagnostics area.
///
/// The three columns `SHOW WARNINGS` returns, and the numbers are
/// MySQL's own: `1265` for a truncated value, `1366` for an integer
/// column given something that is not one. Measured on 9.7.2 rather
/// than looked up, because an errno a client switches on has to be
/// the errno it would have seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlWarning {
    /// `Warning`, `Error` or `Note` — the `Level` column.
    pub level: &'static str,
    /// MySQL's error code.
    pub code: u16,
    /// The message, worded as MySQL words it.
    pub message: String,
}

#[derive(Debug, Default)]
pub(crate) struct SessionBag {
    pub(crate) session_params: BTreeMap<String, String>,
    pub(crate) backslash_escapes: bool,
    /// v7.39 (round 470) — is the MySQL session in a strict `sql_mode`?
    ///
    /// MariaDB's default includes `STRICT_TRANS_TABLES`, so this starts
    /// true; `SET sql_mode=''` (or any list without a STRICT_ flag) turns
    /// it off and a value that would otherwise raise is bent to fit
    /// instead — the same conversion `INSERT IGNORE` uses.
    pub(crate) mysql_strict: bool,
    /// v7.38.18 (C12) — the MySQL diagnostics area: what the last
    /// warning-generating statement bent, ready for `SHOW WARNINGS`
    /// and `@@warning_count`.
    ///
    /// The value-bending half has been byte-for-byte MySQL's since
    /// v7.39 round 470 — `INSERT INTO t (i, s) VALUES ('abc', 'toolong')`
    /// stores `0` and `'too'` exactly as MySQL 9.7.2 does. What was
    /// missing is that MySQL TELLS you: `Warning 1265 Data truncated
    /// for column 's' at row 1`. An application that checks
    /// `@@warning_count` after an insert had no way to learn that its
    /// data had been changed.
    ///
    /// Cleared by the next statement that can produce warnings, which
    /// is MySQL's rule; `SHOW WARNINGS` and `SELECT @@warning_count`
    /// read it without clearing.
    pub(crate) mysql_warnings: Vec<MysqlWarning>,
    pub(crate) prepared_statements: BTreeMap<String, PreparedSqlStatement>,
    /// v7.39 (round 499) — the value `nextval` last returned IN THIS
    /// SESSION, per sequence, and which sequence that was.
    ///
    /// PG defines `currval` and `lastval` as session-local: they answer
    /// the number THIS session was given, and error with 55000 ("not yet
    /// defined in this session") when it has not called `nextval`. They
    /// are deliberately not the sequence's current value — another
    /// session may have advanced it since, and reading that would hand
    /// back a number this session never owned, which is what a caller
    /// then uses as a foreign key.
    ///
    /// Measured before this (`iso_session` T1/T2): `currval` answered in
    /// a connection that had never called `nextval`, and `lastval`
    /// answered across connections, because the tracking lived on the
    /// shared engine rather than in the bag.
    pub(crate) seq_currvals: BTreeMap<String, i64>,
    pub(crate) last_sequence_used: Option<String>,
    /// v7.39 (round 553) — the isolation level THIS connection is
    /// running at.
    ///
    /// It lived on the shared engine, so it leaked both ways between
    /// connections. Measured over pgwire against PG18: connection B
    /// opened a plain BEGIN and `SHOW transaction_isolation` answered
    /// `serializable` — A's level; and A, still inside its SERIALIZABLE
    /// block, then read `read committed`, because B's COMMIT reset the
    /// field under it. PG answers `read committed` and `serializable`
    /// throughout. So a transaction that asked for SERIALIZABLE ran at
    /// READ COMMITTED and one that asked for nothing ran at
    /// SERIALIZABLE, purely because another connection was busy.
    ///
    /// Round 552 saw the same field give way and worked around it by
    /// putting the level on the TRANSACTION; this puts the session's
    /// own copy where the rest of its state already lives — the place
    /// r306's comment says every piece of per-connection state belongs
    /// so it never gets a process-wide version to regress from.
    pub(crate) isolation_level: spg_sql::ast::IsolationLevel,
    /// v7.39 (round 306) — open large-object descriptors. Per session
    /// from the start, deliberately: r277/r279/r283 each landed a piece
    /// of per-connection state on the process-wide engine first and had
    /// to be unpicked afterwards, so this one never gets a process-wide
    /// version to regress from. PG additionally scopes descriptors to
    /// the transaction, so the table is emptied at COMMIT / ROLLBACK.
    pub(crate) lo_descriptors: BTreeMap<i32, LargeObjectDescriptor>,
    /// Next descriptor number to hand out. PG starts at 0 and counts up
    /// within a transaction, restarting once the transaction ends.
    pub(crate) lo_next_fd: i32,
    /// v7.39 (round 321, V54) — open server-side cursors. They lived on
    /// the shared engine until now, i.e. in ONE namespace for every
    /// connection: two clients could not both `DECLARE c`, a `FETCH`
    /// could read another client's rows, and `CLOSE ALL` closed
    /// everybody's.
    pub(crate) cursors: BTreeMap<String, cursor::OpenCursor>,
    /// v7.39 (round 347, M2) — MySQL's `LAST_INSERT_ID()`. Per SESSION
    /// from the start (r277/r279/r283 each paid for landing per-connection
    /// state on the shared engine first): one connection's insert must not
    /// be readable as another's. MariaDB, measured: a fresh session reads
    /// 0; an insert that generates an AUTO_INCREMENT value sets it to the
    /// FIRST one generated; a statement that generates none — an explicit
    /// id, an UPDATE, a DELETE, a plain table — leaves it alone.
    pub(crate) last_insert_id: i64,
    /// v7.39 (round 426) — MySQL's `ROW_COUNT()`. Per SESSION like
    /// `last_insert_id`. MariaDB, measured: a DML statement leaves the
    /// number of rows it CHANGED (an UPDATE that matched but changed
    /// nothing leaves 0); a SELECT leaves -1; DDL leaves 0. A FRESH
    /// session reads 0 (measured), not -1.
    pub(crate) row_count: i64,
    /// v7.39 (round 430) — MySQL USER variables (`SET @x = 5`). Per
    /// SESSION like `last_insert_id` / `row_count`; its own namespace,
    /// separate from the `@@` session parameters. Reading an unset one
    /// answers NULL, as MariaDB does.
    pub(crate) user_vars: BTreeMap<String, spg_storage::Value<'static>>,
    /// v7.39 (round 436) — the logical names of this session's TEMPORARY
    /// tables. Each is stored in the catalog under a per-session prefix; this
    /// set is what says "resolve `t` to my temp one" and what `end_session`
    /// walks to drop them.
    pub(crate) temp_tables: alloc::collections::BTreeSet<String>,
    /// v7.39 (round 469) — the session's TEMPORARY sequences and views, by
    /// logical name. Separate sets because dropping one at session end has
    /// to name the catalog map it lives in.
    pub(crate) temp_sequences: alloc::collections::BTreeSet<String>,
    pub(crate) temp_views: alloc::collections::BTreeSet<String>,
}

/// v7.39 (round 306) — one open large-object descriptor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LargeObjectDescriptor {
    pub(crate) oid: u32,
    /// Byte offset the next read / write starts at.
    pub(crate) pos: u64,
    /// Whether the descriptor was opened with `INV_WRITE`. Reads need no
    /// permission at all in PG — a write-only descriptor reads fine —
    /// so only this half is worth remembering.
    pub(crate) writable: bool,
}

/// v7.39 (round 277) — one SQL-level prepared statement.
#[derive(Debug, Clone)]
pub(crate) struct PreparedSqlStatement {
    /// The body with its `$N` placeholders still in place.
    pub(crate) body: spg_sql::ast::Statement,
    /// Declared parameter type names, in order; empty when PG would
    /// have inferred them.
    pub(crate) param_types: alloc::vec::Vec<String>,
    /// The whole `PREPARE …` text, which `pg_prepared_statements`
    /// reports verbatim.
    pub(crate) source: String,
}

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
/// v7.39 (round 740) — matview delta ground-truth counters (the r735
/// lesson: a green content pin cannot distinguish "delta applied" from
/// "silently fell back to full"; these can).
pub static MATVIEW_FANOUT_BUFFERED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static MATVIEW_DELTA_APPLIED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static MATVIEW_DELTA_BAILED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
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
    /// v7.39 (round 552) — the COMMIT SEQUENCE at which each table was
    /// last written. Commit order, not begin order: a transaction that
    /// began first can commit last, so the writer version allocated at
    /// BEGIN cannot answer "did this change after I read it".
    table_last_commit: BTreeMap<String, u64>,
    /// Monotonic, bumped once per successful COMMIT.
    commit_seq: u64,
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
    /// v7.39 (round 173) — whether the statement-exit trigger runs the
    /// vacuum **inline**. Default ON (embedded: single-threaded host,
    /// the statement path is the only place work can happen). A host
    /// with a background autovacuum worker (spg-server) flips this off
    /// and drives [`Self::autovacuum_tick`] from its own thread instead
    /// — PG's shape, where autovacuum never runs inside a client
    /// statement. Only meaningful while `autovacuum` itself is on.
    autovacuum_inline: bool,
    /// v7.37.15 (Phase C) — TxId → writer version registry. When
    /// `exec_begin` opens an explicit transaction it allocates a
    /// fresh writer version (via [`Self::begin_writer_version`])
    /// and stashes the mapping here so the matching `exec_commit`
    /// / `exec_rollback` can call
    /// [`Self::commit_writer_version`] on the right entry. Empty
    /// when no explicit transactions are open.
    /// v7.39 (round 295, E3 Phase 1b) — rows a `SKIP LOCKED` pass found
    /// held by another transaction. Set by the locking pre-pass and
    /// consulted by the base scan, which is READ-only here: the locks
    /// themselves were taken under `&mut self` in the pre-pass, so the
    /// `&self` scan never mutates the lock table. (A `RefCell` there
    /// would cost `Engine: Sync`, which the server's `RwLock<Engine>`
    /// needs — see the RFC's §5.6.)
    pub(crate) lock_skip_rows: Option<(String, alloc::collections::BTreeSet<usize>)>,
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
    /// v7.39 (round 470) — see [`SessionBag::mysql_strict`].
    mysql_strict: bool,
    /// v7.38.18 (C12) — see [`SessionBag::mysql_warnings`]. Lives on
    /// the Engine like every other live-session field and swaps with
    /// the bag, because the server runs ONE Engine for every
    /// connection and per-connection state that does not swap leaks
    /// across them.
    mysql_warnings: Vec<MysqlWarning>,

    /// v7.38.18 (C12) — the diagnostics area of the statement that is
    /// running RIGHT NOW, which is not the one a read returns. MySQL 9
    /// answers `SELECT @@warning_count` with the PREVIOUS statement's
    /// count and only then replaces the visible set with its own (a
    /// second read in a row therefore answers 0). Producers fill this;
    /// [`Engine::dispatch_stmt_inner`] publishes it into
    /// `mysql_warnings` when the statement ends. Never live across
    /// statements, so it is not part of [`SessionBag`].
    mysql_stmt_warnings: Vec<MysqlWarning>,
    /// v7.39 (round 306) — the live session's open large-object
    /// descriptors, swapped in and out with the rest of its bag.
    pub(crate) lo_descriptors: BTreeMap<i32, LargeObjectDescriptor>,
    pub(crate) lo_next_fd: i32,
    /// v7.37.17 — name of the sequence most recently advanced by
    /// nextval() in this Engine (session). Backs PG's lastval().
    /// None until the first nextval; PG errors in that state.
    last_sequence_used: Option<String>,
    /// v7.39 (round 499) — per-session `currval` values; see
    /// [`SessionBag::seq_currvals`].
    seq_currvals: alloc::collections::BTreeMap<String, i64>,
    /// v7.39 (round 277) — SQL-level prepared statements, session
    /// scoped exactly as in PG. Keyed by name; each entry keeps the
    /// parsed body (placeholders intact), the declared parameter type
    /// names and the statement text `pg_prepared_statements` reports.
    prepared_statements: alloc::collections::BTreeMap<String, PreparedSqlStatement>,
    /// v7.39 (round 279) — which connection's state is currently
    /// installed in the fields above. 0 is the embedded / default
    /// session.
    current_session: u32,
    /// Parked state for every OTHER connection.
    sessions: BTreeMap<u32, SessionBag>,
    /// v7.39 (round 279) — advisory locks, held ACROSS sessions and so
    /// deliberately NOT part of the swapped bag: the whole purpose of
    /// an advisory lock is to be visible to the other connection.
    /// key → (owning session, re-entrant depth). PG allows the same
    /// session to take a lock it already holds.
    advisory_locks: BTreeMap<i64, (u32, u32)>,
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
    /// v7.39 (round 786, T35 Phase A) — host factory for spill runs.
    /// `None` (the default, and every embedded caller that has not opted
    /// in) keeps today's behaviour exactly: a sort that outgrows
    /// `max_query_bytes` still refuses rather than spilling.
    pub(crate) temp_run_factory: Option<crate::TempRunFactory>,
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
    /// v7.39 (round 218) — open server-side cursors (DECLARE … CURSOR),
    /// keyed by name. Materialized at DECLARE (INSENSITIVE semantics —
    /// PG's only actual behaviour too); FETCH / MOVE walk the stored rows.
    /// Lifecycle: created only inside a transaction; COMMIT closes
    /// non-HOLD cursors and marks WITH HOLD ones held; ROLLBACK closes
    /// everything not already held by an earlier commit. Never serialized.
    /// Session-scoped in PG; SPG stores them engine-wide (the same
    /// process-level session-state architecture wall as `session_params`).
    pub(crate) cursors: BTreeMap<String, cursor::OpenCursor>,
    /// v7.39 (round 347, M2) — the current session's LAST_INSERT_ID().
    /// Swapped with [`SessionBag`] like every other per-connection slot.
    /// An atomic because `LAST_INSERT_ID(expr)` SETS it while evaluation
    /// holds only `&Engine` — and `Engine` must stay `Sync`, which a
    /// `Cell` would have taken away (spg-embedded-tokio shares one across
    /// tasks; clippy caught it there before the tests did).
    pub(crate) last_insert_id: core::sync::atomic::AtomicI64,
    /// v7.39 (round 426) — the current session's ROW_COUNT(). Swapped
    /// with [`SessionBag`] like every other per-connection slot. A plain
    /// i64: unlike LAST_INSERT_ID it is only ever WRITTEN from the
    /// statement driver, which holds `&mut Engine`.
    pub(crate) row_count: i64,
    /// v7.39 (round 430) — this session's MySQL USER variables.
    /// Swapped with [`SessionBag`] like every other per-connection slot.
    pub(crate) user_vars: BTreeMap<String, spg_storage::Value<'static>>,
    /// v7.39 (round 436) — the logical names of this session's TEMPORARY
    /// tables. Swapped with [`SessionBag`]; see `session_temp_name`.
    pub(crate) temp_tables: BTreeSet<String>,
    pub(crate) temp_sequences: BTreeSet<String>,
    pub(crate) temp_views: BTreeSet<String>,
    /// v7.39 (round 222) — channels this session LISTENs on. Engine-wide
    /// (the same process-level session-state architecture wall as
    /// `session_params`). Never serialized.
    pub(crate) listen_channels: BTreeSet<String>,
    /// v7.39 (round 222) — NOTIFYs raised inside the current transaction,
    /// held until COMMIT (PG: transactional delivery, deduplicated within
    /// the tx); dropped at ROLLBACK.
    pub(crate) tx_pending_notifies: Vec<(String, String)>,
    /// v7.39 (round 222) — committed notifications on LISTENed channels,
    /// awaiting a drain by the wire layer ('A' NotificationResponse) or an
    /// embedded caller ([`Engine::take_notifications`]).
    pub(crate) delivered_notifies: Vec<(String, String)>,
    /// v7.39 (read01 round 46) — NOTICEs raised by the statement now
    /// executing. PG emits a NoticeResponse whenever an `IF EXISTS` /
    /// `IF NOT EXISTS` clause makes it skip work ("table \"t\" does not
    /// exist, skipping"). The engine appends the PG-worded text here;
    /// the caller drains it with [`Engine::take_notices`] after each
    /// statement (pgwire turns each into an 'N' message, embedded
    /// callers can ignore or surface them). Cleared at the start of
    /// every statement so a notice never leaks into the next one.
    pending_notices: Vec<Notice>,
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
    /// v7.37 (round 884) — what sorts have spilled in this process, for
    /// `pg_stat_database` and for EXPLAIN ANALYZE's `Sort Method`.
    pub(crate) spill_stats: crate::tempstore::SpillStats,
    pub(crate) xact_commit: core::sync::atomic::AtomicU64,
    pub(crate) xact_rollback: core::sync::atomic::AtomicU64,
    /// v7.39 (pg_stat knife A) — host-injected live backend count for
    /// pg_stat_database.numbackends (ClockFn-style fn slot; the server
    /// wires its connection registry, embedded stays None -> 1).
    pub(crate) backend_count_fn: Option<BackendCountFn>,
    pub(crate) backend_pid_fn: Option<BackendPidFn>,
    /// v7.39 (round 476) — see [`WalLsnFn`].
    pub(crate) wal_lsn_fn: Option<WalLsnFn>,
    /// v7.39 (round 318, V51) — host connection-control hook. See
    /// [`BackendSignalFn`].
    pub(crate) backend_signal_fn: Option<BackendSignalFn>,
    /// v7.39 (tz epic) — injected IANA timezone lookups; None on a
    /// host without zoneinfo (named zones then fail to SET, honestly).
    pub(crate) tz_offset_fn: Option<TzOffsetFn>,
    pub(crate) tz_localize_fn: Option<TzLocalizeFn>,
    pub(crate) tz_canon_fn: Option<TzCanonFn>,
    pub(crate) tz_abbrev_fn: Option<TzAbbrevFn>,
    /// v7.39 (round 502) — see [`TzAllFn`].
    pub(crate) tz_all_fn: Option<TzAllFn>,
    pub(crate) stat_tup_inserted: u64,
    pub(crate) stat_tup_updated: u64,
    pub(crate) stat_tup_deleted: u64,
    /// v7.39 (round 192) — per-table DML counters for
    /// pg_stat_user_tables (n_tup_ins / n_tup_upd / n_tup_del).
    /// Engine-side and NON-transactional, like PG's stats collector:
    /// a rolled-back INSERT still counts, and a tx's counts don't
    /// ride the shadow catalog (the RC rebase rebuilt shadow tables
    /// from the committed base, silently dropping any counter bumped
    /// on the shadow — the r192 probe's tx-wrapped inserts read 0).
    /// Keyed by table name; DROP TABLE clears, RENAME re-keys.
    pub(crate) table_write_stats: alloc::collections::BTreeMap<String, (u64, u64, u64)>,
    /// v7.39 (round 196) — bumped after every completed statement that
    /// ran OUTSIDE a transaction block (any autocommit statement, plus
    /// COMMIT itself via the post-statement check). An open tx whose
    /// `rebased_at_epoch` equals this value knows the committed base
    /// hasn't moved and skips the per-statement RC rebase (whose
    /// write-set extraction full-scans every touched table).
    /// Over-approximation is deliberate: read-only statements bump it
    /// too, which only costs an extra (correct) rebase.
    pub(crate) commit_epoch: u64,
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
    /// v7.12.7 — depth counter for trigger-emitted embedded SQL.
    /// Each time the engine executes a `DeferredEmbeddedStmt` it
    /// increments this; the recursive `execute_stmt_with_cancel`
    /// inside that path checks against [`MAX_TRIGGER_RECURSION`]
    /// to bound runaway cascades (trigger A's UPDATE on table B
    /// fires trigger B which UPDATEs table A which fires trigger
    /// A again…). Reset to 0 once the original DML returns.
    trigger_recursion_depth: u32,
    /// v7.39 (round 140) — set while a DELETE / UPDATE is being re-run by the
    /// DO ALSO rule wrapper so the wrapper's inner call does not re-enter the
    /// rule-rewrite path (which would recurse forever). INSERT captures its
    /// post-image rows directly and needs no such guard.
    rule_rewrite_active: bool,
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
    /// v7.39 (round 735, S14/B3) — per-table change sequence, bumped on
    /// every write entry (INSERT / UPDATE / DELETE / TRUNCATE / COPY /
    /// table-shape DDL). In-memory only: after a restart the map is
    /// empty, every watermark comparison misses, and the next REFRESH
    /// is a full one — stale-view-safe by construction. A rolled-back
    /// transaction's bump stays too, which can only cause an EXTRA full
    /// refresh, never a wrong no-op.
    table_change_seq: alloc::collections::BTreeMap<String, u64>,
    /// v7.39 (round 735, S14/B3) — per-materialized-view refresh
    /// watermark: the (table, change-seq) pairs its last full refresh
    /// saw. When every dependency's seq is unchanged, REFRESH is an
    /// O(1) no-op — an incremental-maintenance first step PG does not
    /// have (its REFRESH always recomputes).
    matview_refresh_watermark: alloc::collections::BTreeMap<String, Vec<(String, u64)>>,
    /// v7.39 (round 736, S14/B3 knife 2) — delta-maintainable
    /// materialized views: mv name -> its single base table. Registered
    /// at CREATE MATERIALIZED VIEW / full REFRESH when the body is a
    /// single-stored-table pure projection (no aggregates / joins /
    /// CTEs / subqueries / DISTINCT / ORDER / LIMIT / windows / SRFs).
    matview_maintainable: alloc::collections::BTreeMap<String, String>,
    /// Buffered base-table row changes per maintainable view, fanned
    /// out from the statement redo drain. Capped (see
    /// `MATVIEW_DELTA_CEILING`); an overflowed view falls back to a
    /// full refresh — never-die, never-stale.
    matview_delta_buf: alloc::collections::BTreeMap<String, Vec<RowChange>>,
    matview_delta_overflow: alloc::collections::BTreeSet<String>,
    /// v7.39 (round 738, S14/B3 knife 3) — per-view row map: expected
    /// PHYSICAL length of the view's backing table, plus base-row
    /// RowId -> view row position. Built only by the maintainable full
    /// refresh's internal scan (the SQL path cannot see rowids), and
    /// consulted by the delete/tombstone delta arms. In-memory: restart
    /// or any length mismatch (a vacuum moved rows) -> full refresh.
    matview_row_map:
        alloc::collections::BTreeMap<String, (usize, alloc::collections::BTreeMap<u64, usize>)>,
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
    /// v7.39 (round 319, V52) — the peer's IP, empty when the connection
    /// has no TCP peer (PG reports NULL there).
    pub client_addr: String,
    /// v7.39 (round 319, V52) — the peer's port. PG reports **-1**, not
    /// NULL, for a connection with no TCP port; measured on PG 18.4.
    pub client_port: i32,
    /// v7.39 (round 319, V52) — the database this connection named. Empty
    /// when it named none; both `pg_stat_activity.datname` and
    /// `SHOW PROCESSLIST.db` report that as NULL.
    pub database: String,
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
    /// v7.39 (round 474) — PG's `backend_type`: `client backend` for a
    /// connection, or the worker's own name for a background process.
    ///
    /// pg_stat_activity used to hardcode `client backend`, so SPG's own
    /// background workers — the ones that hold the engine write lock and
    /// are exactly what an operator is looking for when a statement
    /// stalls — did not appear at all. PG18 lists eight of them beside
    /// the single client backend on an idle server.
    pub backend_type: String,
}

impl ActivityRow {
    /// The `backend_type` PG gives a background process: no database, no
    /// user, no query, and a state PG reports as NULL.
    #[must_use]
    pub fn background(pid: u32, backend_type: &str) -> Self {
        Self {
            pid,
            user: String::new(),
            client_addr: String::new(),
            client_port: -1,
            database: String::new(),
            started_at_us: 0,
            current_sql: String::new(),
            wait_event_type: String::new(),
            wait_event: String::new(),
            elapsed_us: 0,
            in_transaction: false,
            application_name: String::new(),
            backend_type: backend_type.into(),
        }
    }
}

/// v6.5.2 — provider callback type. Fresh snapshot returned each
/// call; engine doesn't cache the slice.
pub type ActivityProvider = fn() -> Vec<ActivityRow>;

/// v7.39 (round 318, V41) — how loud a diagnostic the statement raised is.
/// PG distinguishes them on the wire (`S`/`V` fields of NoticeResponse) and
/// clients act on it: psql prints `WARNING:` in a different colour, and
/// several drivers surface warnings to the application while dropping
/// notices. Emitting everything as NOTICE loses that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Notice,
    Warning,
    /// v7.39 (round 757, F31-B3) — `RAISE INFO`. PG sends INFO to the
    /// client ALWAYS, regardless of `client_min_messages`.
    Info,
}

impl NoticeSeverity {
    /// The non-localized severity string PG puts in the `V` field.
    #[must_use]
    pub const fn as_pg_str(self) -> &'static str {
        match self {
            Self::Notice => "NOTICE",
            Self::Warning => "WARNING",
            Self::Info => "INFO",
        }
    }
}

/// v7.39 (round 318, V41) — one diagnostic the statement raised, in PG's
/// exact wording minus the severity banner (the wire layer adds that).
#[derive(Debug, Clone)]
pub struct Notice {
    pub severity: NoticeSeverity,
    pub message: String,
}

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
            table_last_commit: BTreeMap::new(),
            commit_seq: 0,
            current_tx: None,
            backslash_escapes: false,
            mysql_strict: true,
            mysql_warnings: Vec::new(),
            mysql_stmt_warnings: Vec::new(),
            lo_descriptors: BTreeMap::new(),
            lo_next_fd: 0,
            prepared_statements: alloc::collections::BTreeMap::new(),
            current_session: 0,
            sessions: BTreeMap::new(),
            advisory_locks: BTreeMap::new(),
            last_sequence_used: None,
            seq_currvals: alloc::collections::BTreeMap::new(),
            next_tx_id: 1,
            active_writer_versions: BTreeSet::new(),
            aborted_versions: BTreeSet::new(),
            locks: crate::locks::LockTable::new(),
            mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
            autovacuum: true,
            autovacuum_inline: true,
            lock_skip_rows: None,
            tx_writer_versions: BTreeMap::new(),
            stmt_writer_version: None,
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            max_query_bytes: None,
            temp_run_factory: None,
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
            cursors: BTreeMap::new(),
            last_insert_id: core::sync::atomic::AtomicI64::new(0),
            row_count: 0,
            user_vars: BTreeMap::new(),
            temp_tables: BTreeSet::new(),
            temp_sequences: BTreeSet::new(),
            temp_views: BTreeSet::new(),
            listen_channels: BTreeSet::new(),
            tx_pending_notifies: Vec::new(),
            delivered_notifies: Vec::new(),
            pending_notices: Vec::new(),
            spill_stats: crate::tempstore::SpillStats::default(),
            xact_commit: core::sync::atomic::AtomicU64::new(0),
            xact_rollback: core::sync::atomic::AtomicU64::new(0),
            backend_count_fn: None,
            backend_pid_fn: None,
            wal_lsn_fn: None,
            backend_signal_fn: None,
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_canon_fn: None,
            tz_abbrev_fn: None,
            tz_all_fn: None,
            stat_tup_inserted: 0,
            table_write_stats: alloc::collections::BTreeMap::new(),
            commit_epoch: 0,
            stat_tup_updated: 0,
            stat_tup_deleted: 0,
            local_guc_saves: Vec::new(),
            render_style: crate::eval::RenderStyle::default(),
            savepoint_guc_marks: Vec::new(),
            trigger_recursion_depth: 0,
            rule_rewrite_active: false,
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
            table_change_seq: alloc::collections::BTreeMap::new(),
            matview_refresh_watermark: alloc::collections::BTreeMap::new(),
            matview_maintainable: alloc::collections::BTreeMap::new(),
            matview_delta_buf: alloc::collections::BTreeMap::new(),
            matview_delta_overflow: alloc::collections::BTreeSet::new(),
            matview_row_map: alloc::collections::BTreeMap::new(),
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

    /// v7.39 (round 513) — does this role exist? `'x'::regrole` needs it,
    /// and roles live on the engine rather than the catalog.
    #[must_use]
    pub fn role_exists(&self, name: &str) -> bool {
        // v7.39 (round 696) — the SESSION's own identity, same class as the
        // `postgres` case below and missed by it. `current_user` reported
        // the connected name while this predicate denied it, so `SET ROLE
        // <me>` refused the role the session was already running as.
        if name == self.session_user() {
            return true;
        }
        // The engine's default identity exists even before any CREATE USER.
        //
        // v7.39 (round 652) — and so does `postgres`. `synth_pg_roles`
        // has always inserted it as the bootstrap superuser when no user
        // by that name was created, so this predicate and the catalogue
        // it is supposed to reflect disagreed: `pg_roles` listed
        // `postgres` while `'postgres'::regrole` said it did not exist.
        // Every pg_dump names it (`OWNER TO postgres`), so the ALTER
        // TABLE OWNER check added this round would have refused the one
        // role that appears in essentially every dump.
        self.effective_users().contains(name)
            || name.eq_ignore_ascii_case("admin")
            || name.eq_ignore_ascii_case("postgres")
            // 7.38.1 S5.2 — PG's predefined pg_* roles exist without
            // anyone creating them, and pg_dump's ACL section GRANTs
            // to `pg_database_owner` on every dump of `public`.
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "pg_database_owner"
                    | "pg_read_all_data"
                    | "pg_write_all_data"
                    | "pg_monitor"
                    | "pg_read_all_settings"
                    | "pg_read_all_stats"
                    | "pg_stat_scan_tables"
                    | "pg_signal_backend"
                    | "pg_checkpoint"
                    | "pg_maintain"
                    | "pg_use_reserved_connections"
                    | "pg_create_subscription"
            )
    }

    /// v7.39 (round 520) — the role an oid names, as `pg_get_userbyid`
    /// reports it. The numbering is `synth_pg_roles`': base 10, one per
    /// user in catalog order.
    #[must_use]
    pub fn role_name_for_oid(&self, oid: i64) -> Option<String> {
        // Oid 10 is the bootstrap superuser, which `synth_pg_roles` always
        // publishes as `postgres`. Following the catalogue rather than the
        // session is the point: a join on `relowner = pg_roles.oid` and
        // `pg_get_userbyid(relowner)` have to name the same role.
        if oid == 10 {
            return Some(alloc::string::String::from("postgres"));
        }
        let idx = usize::try_from(oid - 11).ok()?;
        self.users
            .iter()
            .nth(idx)
            .map(|(n, _)| alloc::string::String::from(n))
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
        // v7.39 (round 297, E3 Phase 1b) — carry the SKIP LOCKED
        // exclusions on the snapshot. Every row source threads a
        // snapshot through `is_row_visible`, so this is the one place
        // that cannot be routed around; adding the filter per scan site
        // missed the live path three times.
        let locked_out = self.lock_skip_rows.as_ref().and_then(|(t, set)| {
            self.active_catalog()
                .get(t)
                .map(|tbl| (tbl.rel_id(), set.clone()))
        });
        let mut snap = self.current_snapshot_inner();
        snap.locked_out = locked_out;
        snap
    }

    fn current_snapshot_inner(&self) -> spg_storage::snapshot::Snapshot {
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

    /// 7.38.1 S2.1 — drop the tuple locks an AUTOCOMMIT statement took
    /// (its implicit transaction ends with it). A no-op inside an open
    /// transaction (those release at COMMIT/ROLLBACK) and when the
    /// statement allocated no writer version.
    pub(crate) fn release_autocommit_stmt_locks(&mut self) {
        let in_tx = self
            .current_tx
            .is_some_and(|tx| self.tx_writer_versions.contains_key(&tx));
        if in_tx {
            return;
        }
        if let Some(v) = self.stmt_writer_version {
            self.locks.release_all(v);
        }
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
    /// v7.39 (round 476) — register the WAL byte-position provider.
    pub fn set_wal_lsn_fn(&mut self, f: WalLsnFn) {
        self.wal_lsn_fn = Some(f);
    }

    pub fn set_backend_pid_fn(&mut self, f: BackendPidFn) {
        self.backend_pid_fn = Some(f);
    }

    /// v7.39 (round 318, V51) — inject the host's connection-control hook,
    /// so `pg_cancel_backend` / `pg_terminate_backend` / `KILL` act instead
    /// of answering a constant.
    pub fn set_backend_signal_fn(&mut self, f: BackendSignalFn) {
        self.backend_signal_fn = Some(f);
    }

    /// v7.39 (round 786, T35 Phase A) — install the host's spill-run
    /// factory. Without one the engine cannot spill and a sort that
    /// outgrows `max_query_bytes` keeps refusing, which is exactly the
    /// behaviour every caller has today.
    pub fn set_temp_run_factory(&mut self, f: crate::TempRunFactory) {
        self.temp_run_factory = Some(f);
    }

    /// Whether spilling is available in this process.
    #[must_use]
    pub fn can_spill(&self) -> bool {
        self.temp_run_factory.is_some()
    }

    /// v7.39 (round 786) — open a fresh spill run, or `None` when no
    /// host factory is installed. Phase B's run generation calls this;
    /// it lives here so the `None` path stays a single decision point.
    pub(crate) fn open_temp_run(
        &self,
    ) -> Option<Result<alloc::boxed::Box<dyn crate::TempRun>, crate::TempStoreError>> {
        self.temp_run_factory.map(|f| f())
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

    /// v7.39 (round 502) — the zone enumerator behind `pg_timezone_names`.
    /// Separate from `set_tz_fns` so an embedder that already calls that
    /// one keeps compiling.
    pub fn set_tz_all_fn(&mut self, all: TzAllFn) {
        self.tz_all_fn = Some(all);
    }

    /// Every zone the host knows at `utc_micros`; empty without a hook.
    pub(crate) fn tz_all_at(
        &self,
        utc_micros: i64,
    ) -> alloc::vec::Vec<(alloc::string::String, alloc::string::String, i64, bool)> {
        self.tz_all_fn
            .map_or_else(alloc::vec::Vec::new, |f| f(utc_micros))
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
            lock_skip_rows: None,
            catalog,
            parallel_runner: ParallelRunnerSlot::default(),
            tx_catalogs: BTreeMap::new(),
            table_last_commit: BTreeMap::new(),
            commit_seq: 0,
            current_tx: None,
            backslash_escapes: false,
            mysql_strict: true,
            mysql_warnings: Vec::new(),
            mysql_stmt_warnings: Vec::new(),
            lo_descriptors: BTreeMap::new(),
            lo_next_fd: 0,
            prepared_statements: alloc::collections::BTreeMap::new(),
            current_session: 0,
            sessions: BTreeMap::new(),
            advisory_locks: BTreeMap::new(),
            last_sequence_used: None,
            seq_currvals: alloc::collections::BTreeMap::new(),
            next_tx_id: 1,
            active_writer_versions: BTreeSet::new(),
            aborted_versions: BTreeSet::new(),
            locks: crate::locks::LockTable::new(),
            mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
            autovacuum: true,
            autovacuum_inline: true,
            tx_writer_versions: BTreeMap::new(),
            stmt_writer_version: None,
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            max_query_bytes: None,
            temp_run_factory: None,
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
            cursors: BTreeMap::new(),
            last_insert_id: core::sync::atomic::AtomicI64::new(0),
            row_count: 0,
            user_vars: BTreeMap::new(),
            temp_tables: BTreeSet::new(),
            temp_sequences: BTreeSet::new(),
            temp_views: BTreeSet::new(),
            listen_channels: BTreeSet::new(),
            tx_pending_notifies: Vec::new(),
            delivered_notifies: Vec::new(),
            pending_notices: Vec::new(),
            spill_stats: crate::tempstore::SpillStats::default(),
            xact_commit: core::sync::atomic::AtomicU64::new(0),
            xact_rollback: core::sync::atomic::AtomicU64::new(0),
            backend_count_fn: None,
            backend_pid_fn: None,
            wal_lsn_fn: None,
            backend_signal_fn: None,
            tz_offset_fn: None,
            tz_localize_fn: None,
            tz_canon_fn: None,
            tz_abbrev_fn: None,
            tz_all_fn: None,
            stat_tup_inserted: 0,
            table_write_stats: alloc::collections::BTreeMap::new(),
            commit_epoch: 0,
            stat_tup_updated: 0,
            stat_tup_deleted: 0,
            local_guc_saves: Vec::new(),
            render_style: crate::eval::RenderStyle::default(),
            savepoint_guc_marks: Vec::new(),
            trigger_recursion_depth: 0,
            rule_rewrite_active: false,
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
            table_change_seq: alloc::collections::BTreeMap::new(),
            matview_refresh_watermark: alloc::collections::BTreeMap::new(),
            matview_maintainable: alloc::collections::BTreeMap::new(),
            matview_delta_buf: alloc::collections::BTreeMap::new(),
            matview_delta_overflow: alloc::collections::BTreeSet::new(),
            matview_row_map: alloc::collections::BTreeMap::new(),
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
                let mut catalog =
                    Catalog::deserialize(catalog_bytes).map_err(EngineError::Storage)?;
                crate::ddl::rebuild_all_excl_indexes(&mut catalog);
                // v7.38.16 — and refill the expression indexes, whose keys
                // the format cannot carry: what is on disk under one was
                // written by a version that stored the wrong values there.
                crate::expr_index::rebuild_all(&mut catalog);
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
                    lock_skip_rows: None,
                    catalog,
                    parallel_runner: ParallelRunnerSlot::default(),
                    tx_catalogs: BTreeMap::new(),
                    table_last_commit: BTreeMap::new(),
                    commit_seq: 0,
                    current_tx: None,
                    backslash_escapes: false,
                    mysql_strict: true,
                    mysql_warnings: Vec::new(),
                    mysql_stmt_warnings: Vec::new(),
                    lo_descriptors: BTreeMap::new(),
                    lo_next_fd: 0,
                    prepared_statements: alloc::collections::BTreeMap::new(),
                    current_session: 0,
                    sessions: BTreeMap::new(),
                    advisory_locks: BTreeMap::new(),
                    last_sequence_used: None,
                    seq_currvals: alloc::collections::BTreeMap::new(),
                    next_tx_id: 1,
                    active_writer_versions: BTreeSet::new(),
                    aborted_versions: BTreeSet::new(),
                    locks: crate::locks::LockTable::new(),
                    mvcc_inplace: !cfg!(feature = "mvcc-inplace-off"),
                    autovacuum: true,
                    autovacuum_inline: true,
                    tx_writer_versions: BTreeMap::new(),
                    stmt_writer_version: None,
                    clock: None,
                    salt_fn: None,
                    max_query_rows: None,
                    max_query_bytes: None,
                    temp_run_factory: None,
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
                    cursors: BTreeMap::new(),
                    last_insert_id: core::sync::atomic::AtomicI64::new(0),
                    row_count: 0,
                    user_vars: BTreeMap::new(),
                    temp_tables: BTreeSet::new(),
                    temp_sequences: BTreeSet::new(),
                    temp_views: BTreeSet::new(),
                    listen_channels: BTreeSet::new(),
                    tx_pending_notifies: Vec::new(),
                    delivered_notifies: Vec::new(),
                    pending_notices: Vec::new(),
                    spill_stats: crate::tempstore::SpillStats::default(),
                    xact_commit: core::sync::atomic::AtomicU64::new(0),
                    xact_rollback: core::sync::atomic::AtomicU64::new(0),
                    backend_count_fn: None,
                    backend_pid_fn: None,
                    wal_lsn_fn: None,
                    backend_signal_fn: None,
                    tz_offset_fn: None,
                    tz_localize_fn: None,
                    tz_canon_fn: None,
                    tz_abbrev_fn: None,
                    tz_all_fn: None,
                    stat_tup_inserted: 0,
                    table_write_stats: alloc::collections::BTreeMap::new(),
                    commit_epoch: 0,
                    stat_tup_updated: 0,
                    stat_tup_deleted: 0,
                    local_guc_saves: Vec::new(),
                    render_style: crate::eval::RenderStyle::default(),
                    savepoint_guc_marks: Vec::new(),
                    trigger_recursion_depth: 0,
                    rule_rewrite_active: false,
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
                    table_change_seq: alloc::collections::BTreeMap::new(),
                    matview_refresh_watermark: alloc::collections::BTreeMap::new(),
                    matview_maintainable: alloc::collections::BTreeMap::new(),
                    matview_delta_buf: alloc::collections::BTreeMap::new(),
                    matview_delta_overflow: alloc::collections::BTreeSet::new(),
                    matview_row_map: alloc::collections::BTreeMap::new(),
                })
            }
            EnvelopeParse::CrcMismatch { expected, computed } => {
                Err(EngineError::Storage(StorageError::Corrupt(alloc::format!(
                    "snapshot envelope CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
                ))))
            }
            EnvelopeParse::Bare => {
                let mut catalog = Catalog::deserialize(buf).map_err(EngineError::Storage)?;
                crate::ddl::rebuild_all_excl_indexes(&mut catalog);
                // v7.38.16 — and refill the expression indexes, whose keys
                // the format cannot carry: what is on disk under one was
                // written by a version that stored the wrong values there.
                crate::expr_index::rebuild_all(&mut catalog);
                Ok(Self::restore(catalog))
            }
        }
    }

    pub const fn users(&self) -> &UserStore {
        &self.users
    }

    /// Builder: attach a wall clock so `NOW()` / `CURRENT_TIMESTAMP` /
    /// `CURRENT_DATE` evaluate to a real value instead of erroring out.
    /// v7.39 (round 279) — announce which connection is about to run.
    /// The server calls this before every statement; embedded hosts
    /// never do and stay on session 0.
    ///
    /// Swapping parks the outgoing connection's state and installs the
    /// incoming one's, creating it on first sight. The plan cache is
    /// cleared because the string-literal dialect is part of what
    /// swaps and the same SQL text lexes differently under it.
    pub fn set_current_session(&mut self, id: u32) {
        if id == self.current_session {
            return;
        }
        let outgoing = SessionBag {
            session_params: core::mem::take(&mut self.session_params),
            backslash_escapes: self.backslash_escapes,
            mysql_strict: self.mysql_strict,
            mysql_warnings: core::mem::take(&mut self.mysql_warnings),
            prepared_statements: core::mem::take(&mut self.prepared_statements),
            lo_descriptors: core::mem::take(&mut self.lo_descriptors),
            lo_next_fd: self.lo_next_fd,
            cursors: core::mem::take(&mut self.cursors),
            last_insert_id: self
                .last_insert_id
                .load(core::sync::atomic::Ordering::Relaxed),
            row_count: self.row_count,
            user_vars: core::mem::take(&mut self.user_vars),
            temp_tables: core::mem::take(&mut self.temp_tables),
            temp_sequences: core::mem::take(&mut self.temp_sequences),
            temp_views: core::mem::take(&mut self.temp_views),
            seq_currvals: core::mem::take(&mut self.seq_currvals),
            last_sequence_used: self.last_sequence_used.take(),
            isolation_level: self.current_isolation_level,
        };
        self.sessions.insert(self.current_session, outgoing);
        let incoming = self.sessions.remove(&id).unwrap_or_default();
        self.session_params = incoming.session_params;
        self.backslash_escapes = incoming.backslash_escapes;
        self.mysql_strict = incoming.mysql_strict;
        self.mysql_warnings = incoming.mysql_warnings;
        self.prepared_statements = incoming.prepared_statements;
        self.lo_descriptors = incoming.lo_descriptors;
        self.lo_next_fd = incoming.lo_next_fd;
        self.cursors = incoming.cursors;
        self.last_insert_id.store(
            incoming.last_insert_id,
            core::sync::atomic::Ordering::Relaxed,
        );
        self.row_count = incoming.row_count;
        self.user_vars = incoming.user_vars;
        self.temp_tables = incoming.temp_tables;
        self.temp_sequences = incoming.temp_sequences;
        self.temp_views = incoming.temp_views;
        self.seq_currvals = incoming.seq_currvals;
        self.last_sequence_used = incoming.last_sequence_used;
        self.current_isolation_level = incoming.isolation_level;
        self.current_session = id;
        // The incoming session's temp namespace must be live before its very
        // first statement resolves a name.
        self.refresh_temp_prefix();
        self.plan_cache.clear();
    }

    /// v7.39 (round 436) — the catalog-name prefix session `id` stores its
    /// TEMPORARY tables under. Mirrors PG's per-session `pg_temp_N` schema;
    /// the leading underscores keep it out of any name a client can write.
    fn temp_prefix_for(id: u32) -> String {
        alloc::format!("__spg_temp_{id}__")
    }

    /// v7.38.14 — which session's temporary namespace does this catalog
    /// name belong to, if any? The inverse of `session_temp_name`, so the
    /// system catalog can report `pg_temp_N` for it instead of `public`.
    pub(crate) fn temp_session_of(catalog_name: &str) -> Option<u32> {
        let rest = catalog_name.strip_prefix("__spg_temp_")?;
        let (id, _) = rest.split_once("__")?;
        id.parse().ok()
    }

    /// The catalog name this session's TEMPORARY table `logical` takes.
    pub(crate) fn session_temp_name(&self, logical: &str) -> String {
        alloc::format!("{}{logical}", Self::temp_prefix_for(self.current_session))
    }

    /// v7.39 (round 436) — point every catalog this session can reach at its
    /// temp namespace, or at none when it owns no temporary tables (so a
    /// session that never made one pays a single `Option` check per lookup).
    /// Both the committed catalog and any open transaction's shadow are set:
    /// a temp table created inside a transaction must resolve there too.
    pub(crate) fn refresh_temp_prefix(&mut self) {
        let prefix = if self.temp_tables.is_empty()
            && self.temp_sequences.is_empty()
            && self.temp_views.is_empty()
        {
            None
        } else {
            Some(Self::temp_prefix_for(self.current_session))
        };
        self.catalog.set_temp_prefix(prefix.clone());
        for shadow in self.tx_catalogs.values_mut() {
            shadow.catalog.set_temp_prefix(prefix.clone());
        }
    }

    /// v7.39 (round 279) — a connection has gone away: drop its parked
    /// state and release every advisory lock it still held, which is
    /// what PG does at backend exit.
    pub fn end_session(&mut self, id: u32) {
        // v7.39 (round 436) — a TEMPORARY table dies with its session, in
        // both PG and MySQL. Done before the bag is dropped, since the bag
        // is what knows which tables the session owns.
        let owned: Vec<String> = if id == self.current_session {
            self.temp_tables.iter().cloned().collect()
        } else {
            self.sessions
                .get(&id)
                .map(|b| b.temp_tables.iter().cloned().collect())
                .unwrap_or_default()
        };
        // v7.39 (round 469) — the same for TEMPORARY sequences and views,
        // which PG also drops at backend exit.
        let owned_seqs: Vec<String> = if id == self.current_session {
            self.temp_sequences.iter().cloned().collect()
        } else {
            self.sessions
                .get(&id)
                .map(|b| b.temp_sequences.iter().cloned().collect())
                .unwrap_or_default()
        };
        let owned_views: Vec<String> = if id == self.current_session {
            self.temp_views.iter().cloned().collect()
        } else {
            self.sessions
                .get(&id)
                .map(|b| b.temp_views.iter().cloned().collect())
                .unwrap_or_default()
        };
        if !owned.is_empty() || !owned_seqs.is_empty() || !owned_views.is_empty() {
            let prefix = Self::temp_prefix_for(id);
            for logical in owned {
                let mangled = alloc::format!("{prefix}{logical}");
                self.catalog.drop_table(&mangled);
            }
            for logical in owned_seqs {
                let mangled = alloc::format!("{prefix}{logical}");
                self.catalog.drop_sequence(&mangled);
            }
            for logical in owned_views {
                let mangled = alloc::format!("{prefix}{logical}");
                self.catalog.drop_view(&mangled);
            }
            if id == self.current_session {
                self.temp_tables.clear();
                self.temp_sequences.clear();
                self.temp_views.clear();
                self.refresh_temp_prefix();
            }
        }
        self.sessions.remove(&id);
        self.advisory_locks.retain(|_, (owner, _)| *owner != id);
        if id == self.current_session {
            self.session_params.clear();
            self.prepared_statements.clear();
            self.backslash_escapes = false;
            self.mysql_strict = true;
            self.lo_descriptors.clear();
            self.lo_next_fd = 0;
            self.cursors.clear();
            self.current_session = 0;
        }
    }

    /// v7.39 (round 302, V15) — force the current session's string-literal
    /// dialect. A MySQL-protocol connection defaults to MySQL semantics
    /// (backslash is an escape: `'\n'` is a newline), which PG's own
    /// default (`standard_conforming_strings = on`) does not do. The
    /// mysql-wire shim calls this once, right after installing its
    /// session, so a client that never sends `SET sql_mode` still gets
    /// MySQL string handling; a later `SET sql_mode='NO_BACKSLASH_ESCAPES'`
    /// flips it back through the normal SET path. Clearing the plan cache
    /// mirrors [`set_current_session`] — the same SQL text lexes
    /// differently once the flag moves.
    /// Is this session in MySQL dialect right now?
    ///
    /// v7.38.17 — a test harness cannot take a file's word for which
    /// dialect it runs in. `SET sql_mode = 'STRICT_TRANS_TABLES'` puts a
    /// session into MySQL semantics mid-file, so six corpus files were
    /// entering MySQL through a door no directive named, and a harness
    /// that read the directive reported them as PostgreSQL. Ask the
    /// session, do not parse the script.
    pub fn in_mysql_dialect(&self) -> bool {
        self.backslash_escapes
    }

    pub fn set_backslash_escapes(&mut self, flag: bool) {
        if flag != self.backslash_escapes {
            self.backslash_escapes = flag;
            self.plan_cache.clear();
        }
    }

    /// v7.39 (round 279) — take an advisory lock. Returns false only
    /// when ANOTHER session holds it; re-taking one this session
    /// already holds bumps a depth counter, as in PG.
    pub(crate) fn advisory_try_lock(&mut self, key: i64) -> bool {
        let me = self.current_session;
        match self.advisory_locks.get_mut(&key) {
            Some((owner, depth)) if *owner == me => {
                *depth += 1;
                true
            }
            Some(_) => false,
            None => {
                self.advisory_locks.insert(key, (me, 1));
                true
            }
        }
    }

    /// Release one level. False when this session does not hold it —
    /// PG answers false and emits a warning; SPG answers false.
    pub(crate) fn advisory_unlock(&mut self, key: i64) -> bool {
        let me = self.current_session;
        match self.advisory_locks.get_mut(&key) {
            Some((owner, depth)) if *owner == me => {
                *depth -= 1;
                if *depth == 0 {
                    self.advisory_locks.remove(&key);
                }
                true
            }
            _ => false,
        }
    }

    /// Release every advisory lock this session holds.
    pub(crate) fn advisory_unlock_all(&mut self) {
        let me = self.current_session;
        self.advisory_locks.retain(|_, (owner, _)| *owner != me);
    }

    /// v7.39 (round 417) — the current session's id (for MySQL
    /// `IS_USED_LOCK`, which reports the connection that holds a lock).
    /// v7.39 (round 430) — read one of this session's MySQL USER
    /// variables. `None` when it was never set, which the caller turns
    /// into NULL (MariaDB reads an unset user variable as NULL).
    pub(crate) fn user_var(&self, name: &str) -> Option<&spg_storage::Value<'static>> {
        self.user_vars.get(name)
    }

    pub(crate) const fn current_session_id(&self) -> u32 {
        self.current_session
    }

    /// v7.39 (round 417) — who holds an advisory-lock key (any session id),
    /// or `None` when nobody holds it. Used by MySQL `IS_USED_LOCK` and to
    /// separate `RELEASE_LOCK`'s "not held by anyone" (returns NULL) from
    /// "held by someone else" (returns 0).
    pub(crate) fn advisory_holder(&self, key: i64) -> Option<u32> {
        self.advisory_locks.get(&key).map(|(owner, _)| *owner)
    }

    /// v7.39 (round 417) — MySQL `RELEASE_ALL_LOCKS()` returns the number of
    /// locks it released; PG's `pg_advisory_unlock_all()` returns void.
    pub(crate) fn advisory_unlock_all_count(&mut self) -> i32 {
        let me = self.current_session;
        // Total depth held by this session, so re-locked keys count as many.
        let mut n: i32 = 0;
        for (_, (owner, depth)) in &self.advisory_locks {
            if *owner == me {
                n = n.saturating_add(*depth as i32);
            }
        }
        self.advisory_locks.retain(|_, (owner, _)| *owner != me);
        n
    }

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
        // r1051 — a configured seed reaches `random()` too, not only
        // the `rng_seed()` accessor: the D-mechanism's "single seed
        // source" claim, made true at the SQL level (first pin_v738_
        // catch). Production engines carry no seed and never touch
        // the PRNG state here.
        if let Some(seed) = env_cfg.random_seed {
            crate::eval::math::prng_install_seed(seed);
        }
        // r1058 — `SPG_TEST_FIXED_CLOCK_MICROS` pins the clock for
        // hosts that read GUCs from env instead of calling
        // `with_clock` (the server). Process-global by necessity: a
        // `ClockFn` is a plain fn pointer and cannot capture.
        if let Some(micros) = env_cfg.fixed_clock_micros {
            FIXED_CLOCK_MICROS.store(micros, core::sync::atomic::Ordering::Relaxed);
            self = self.with_clock(fixed_clock_from_env);
        }
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

    /// v7.37 (round 828) — the user store THIS session should read:
    /// its transaction's role shadow when one exists, the committed
    /// store otherwise. The auth path and other sessions read
    /// `self.users` directly on purpose — an uncommitted role must not
    /// be visible to them, let alone able to log in.
    pub(crate) fn effective_users(&self) -> &crate::users::UserStore {
        if let Some(tx) = self.current_tx
            && let Some(state) = self.tx_catalogs.get(&tx)
            && let Some(shadow) = &state.users
        {
            return shadow;
        }
        &self.users
    }

    /// v7.37 (round 828) — the store role DDL writes to: the TX's role
    /// shadow (created from the committed store on first use) inside a
    /// transaction, the committed store in autocommit. Every mutation
    /// of roles or memberships goes through here, so `BEGIN; CREATE
    /// ROLE r; ROLLBACK` leaves nothing behind — the shadow drops with
    /// the TxState — and COMMIT installs the shadow wholesale.
    pub(crate) fn role_ddl_users_mut(&mut self) -> &mut crate::users::UserStore {
        let tx_slot = self
            .current_tx
            .filter(|tx| self.tx_catalogs.contains_key(tx));
        match tx_slot {
            Some(tx) => {
                if self
                    .tx_catalogs
                    .get(&tx)
                    .is_some_and(|state| state.users.is_none())
                {
                    let committed = self.users.clone();
                    if let Some(state) = self.tx_catalogs.get_mut(&tx) {
                        state.users = Some(committed);
                    }
                }
                self.tx_catalogs
                    .get_mut(&tx)
                    .and_then(|state| state.users.as_mut())
                    .expect("role shadow ensured just above for an open tx slot")
            }
            None => &mut self.users,
        }
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

    /// v7.39 (round 598) — mutable access to the base catalog, for the
    /// recursive-CTE loop.
    ///
    /// It built a whole `Engine` per iteration to hold the working set:
    /// `Engine::restore` initialises 82 fields, and a counting allocator put
    /// the loop at 63 allocations and 104 kB PER ITERATION — 1 GB for a
    /// 10,000-row recursive CTE, none of it dependent on how much else was
    /// in the catalog. One engine, whose CTE table is refilled each round,
    /// needs this.
    pub(crate) fn base_catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    /// v7.38.18 (S1) — the collation this database was created with.
    pub fn database_collation(&self) -> &str {
        self.active_catalog().db_collation()
    }

    /// Record the collation this database is created with, once.
    ///
    /// The host calls this before the first table exists, from its
    /// environment — `LC_ALL`, then `LC_COLLATE`, then `LANG`, the order
    /// `initdb` reads them in. An existing database keeps what it was
    /// created with and this answers `Ok(false)`; asking for a DIFFERENT
    /// collation on a database that already has tables is an error, the
    /// same one PostgreSQL gives, because every index key in it was
    /// built under the collation it has.
    ///
    /// # Errors
    /// When the database already has a different collation in force.
    pub fn set_database_collation(&mut self, name: &str) -> Result<bool, EngineError> {
        // v7.38.18 (G2) — a name PostgreSQL does not have is not a
        // collation. Before this, ICU's fallback to root made `zz_ZZ` a
        // perfectly acceptable database collation.
        if !crate::collate::is_known(name) {
            return Err(crate::collate::unknown_collation_error(name));
        }
        if !crate::collate::is_supported(name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "collation {name:?} is not one this build can perform; \
                 a database created under it could not be read back"
            )));
        }
        let changed = self
            .catalog
            .set_db_collation(name)
            .map_err(EngineError::Storage)?;
        Ok(changed)
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
                Some(s) => {
                    // v7.39 (round 494) — see `TxState::shadow_dirty`.
                    s.shadow_dirty = true;
                    &mut s.catalog
                }
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

    /// v7.39 (round 735, S14/B3) — record that `table`'s rows (or shape)
    /// changed. Cheap (one BTreeMap bump), called from every write entry;
    /// the materialized-view refresh watermark reads it.
    /// v7.39 (round 736) — per-view buffered-delta ceiling. Past this,
    /// the view's next REFRESH is a full one (the buffer is the
    /// optimisation, not the truth).
    pub(crate) const MATVIEW_DELTA_CEILING: usize = 65_536;

    /// v7.39 (round 736) — fan the drained redo out to every
    /// maintainable view whose base table it touches.
    pub(crate) fn fan_out_matview_deltas(&mut self, drained: &[RowChange]) {
        if self.matview_maintainable.is_empty() {
            return;
        }
        for ch in drained {
            let t = ch.table_name().to_ascii_lowercase();
            let hit: Vec<String> = self
                .matview_maintainable
                .iter()
                .filter(|(_, base)| **base == t)
                .map(|(mv, _)| mv.clone())
                .collect();
            for mv in hit {
                if self.matview_delta_overflow.contains(&mv) {
                    continue;
                }
                let buf = self.matview_delta_buf.entry(mv.clone()).or_default();
                if buf.len() >= Self::MATVIEW_DELTA_CEILING {
                    self.matview_delta_overflow.insert(mv.clone());
                    self.matview_delta_buf.remove(&mv);
                } else {
                    buf.push(ch.clone());
                    MATVIEW_FANOUT_BUFFERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    pub(crate) fn bump_table_change(&mut self, table: &str) {
        let k = table.to_ascii_lowercase();
        *self.table_change_seq.entry(k).or_insert(0) += 1;
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

impl Engine {
    /// v7.38.18 (C12) — how many warnings the last warning-generating
    /// statement produced: `@@warning_count`, and what a wire shim
    /// reports in its OK packet.
    #[must_use]
    pub fn mysql_warning_count(&self) -> usize {
        self.mysql_warnings.len()
    }

    /// The diagnostics area itself, for `SHOW WARNINGS`.
    #[must_use]
    pub fn mysql_warnings(&self) -> &[MysqlWarning] {
        &self.mysql_warnings
    }
}

#[cfg(test)]
mod tests;
