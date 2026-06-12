//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
pub mod copy;
pub mod describe;
pub mod eval;
pub mod fts;
pub mod json;
pub mod memoize;
pub mod plan_cache;
pub mod publications;
pub mod query_stats;
pub mod reorder;
pub mod selectivity;
pub mod statistics;
pub mod subscriptions;
pub mod triggers;
pub mod users;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement, CreatePublicationStatement,
    CreateSubscriptionStatement, CreateTableStatement, CreateUserStatement, Expr, FrameBound,
    FrameKind, FromClause, IndexMethod, InsertStatement, JoinKind, Literal, OrderBy, SelectItem,
    SelectStatement, Statement, TableRef, UnOp, UnionKind, VecEncoding as SqlVecEncoding,
    WindowFrame,
};
// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::Statement as ParsedStatement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{
    Catalog, ColumnSchema, CompactReport, DataType, IndexKey, IndexKind, Row, StorageError, Table,
    TableSchema, Value, VecEncoding,
};

use crate::eval::{EvalContext, EvalError};

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

// ---- snapshot envelope (v4.1, extended with CRC32 in v4.37,  ----
// ----   publications in v6.1.2 v3, subscriptions in v6.1.4 v4) ----
//
// Wraps a catalog blob + a user blob behind a small header so the
// server can persist both atomically without inventing a new file.
// Bare catalog blobs (v3.x) still load via `restore_envelope` since
// the magic check fails fast and the function falls back to
// `Catalog::deserialize`.
//
// Layout — v1 (v4.1, no CRC):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 1]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//
// Layout — v2 (v4.37, CRC32 of body):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 2]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//   [u32 crc32]                      ← CRC32 of every byte before it.
//
// Layout — v3 (v6.1.2, publications trailer):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 3]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//   [u32 pubs_len][publications bytes]
//   [u32 crc32]
//
// Layout — v4 (v6.1.4, subscriptions trailer):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 4]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//   [u32 pubs_len][publications bytes]
//   [u32 subs_len][subscriptions bytes]
//   [u32 crc32]
//
// Layout — v5 (v6.2.0, statistics trailer):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 5]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//   [u32 pubs_len][publications bytes]
//   [u32 subs_len][subscriptions bytes]
//   [u32 stats_len][statistics bytes]      ← NEW
//   [u32 crc32]
//
// Writers emit v5 from v6.2.0 on. Readers accept all of {v1, v2,
// v3, v4, v5}: v1/v2 load with empty publications / subscriptions /
// statistics; v3 loads with empty subscriptions + statistics; v4
// loads with empty statistics; v5 deserialises all three. Older
// SPG versions reading a v5 envelope fall through the version
// match to `EnvelopeParse::Bare` — pre-v6.2.0 binaries cannot
// open v6.2.0+ snapshots (matches the v6.1.2 / v6.1.4 breaks).

const ENVELOPE_MAGIC: &[u8; 8] = b"SPGENV01";
const ENVELOPE_VERSION_V1: u8 = 1;
const ENVELOPE_VERSION_V2: u8 = 2;
const ENVELOPE_VERSION_V3: u8 = 3;
const ENVELOPE_VERSION_V4: u8 = 4;
const ENVELOPE_VERSION_V5: u8 = 5;

fn build_envelope(catalog: &[u8], users: &[u8], pubs: &[u8], subs: &[u8], stats: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        8 + 1
            + 4
            + catalog.len()
            + 4
            + users.len()
            + 4
            + pubs.len()
            + 4
            + subs.len()
            + 4
            + stats.len()
            + 4,
    );
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.push(ENVELOPE_VERSION_V5);
    out.extend_from_slice(
        &u32::try_from(catalog.len())
            .expect("≤ 4G catalog")
            .to_le_bytes(),
    );
    out.extend_from_slice(catalog);
    out.extend_from_slice(
        &u32::try_from(users.len())
            .expect("≤ 4G users")
            .to_le_bytes(),
    );
    out.extend_from_slice(users);
    out.extend_from_slice(
        &u32::try_from(pubs.len())
            .expect("≤ 4G publications")
            .to_le_bytes(),
    );
    out.extend_from_slice(pubs);
    out.extend_from_slice(
        &u32::try_from(subs.len())
            .expect("≤ 4G subscriptions")
            .to_le_bytes(),
    );
    out.extend_from_slice(subs);
    out.extend_from_slice(
        &u32::try_from(stats.len())
            .expect("≤ 4G statistics")
            .to_le_bytes(),
    );
    out.extend_from_slice(stats);
    let crc = spg_crypto::crc32::crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Outcome of envelope parsing: either bare-catalog fallback, a
/// successfully split section trio from a v1/v2/v3 envelope, or an
/// explicit corruption error from a v2/v3 CRC mismatch. `Bare`
/// (catalog-only fallback) preserves v3.x readability. v1/v2
/// envelopes set `publications` to `None`; v3 sets it to the
/// publications byte slice.
enum EnvelopeParse<'a> {
    Bare,
    Pair {
        catalog: &'a [u8],
        users: &'a [u8],
        publications: Option<&'a [u8]>,
        subscriptions: Option<&'a [u8]>,
        statistics: Option<&'a [u8]>,
    },
    CrcMismatch {
        expected: u32,
        computed: u32,
    },
}

/// Returns `EnvelopeParse::Pair` for a valid v1 / v2 / v3 envelope,
/// `Bare` for a buffer that doesn't look like an envelope (v3.x
/// bare catalog fallback), and `CrcMismatch` for a v2/v3 envelope
/// whose trailing CRC32 doesn't match the body.
fn split_envelope(buf: &[u8]) -> EnvelopeParse<'_> {
    if buf.len() < 8 + 1 + 4 || &buf[..8] != ENVELOPE_MAGIC {
        return EnvelopeParse::Bare;
    }
    let version = buf[8];
    if !matches!(
        version,
        ENVELOPE_VERSION_V1
            | ENVELOPE_VERSION_V2
            | ENVELOPE_VERSION_V3
            | ENVELOPE_VERSION_V4
            | ENVELOPE_VERSION_V5
    ) {
        return EnvelopeParse::Bare;
    }
    let mut p = 9usize;
    let Some(cat_len_bytes) = buf.get(p..p + 4) else {
        return EnvelopeParse::Bare;
    };
    let Ok(cat_len_arr) = cat_len_bytes.try_into() else {
        return EnvelopeParse::Bare;
    };
    let cat_len = u32::from_le_bytes(cat_len_arr) as usize;
    p += 4;
    if p + cat_len + 4 > buf.len() {
        return EnvelopeParse::Bare;
    }
    let catalog = &buf[p..p + cat_len];
    p += cat_len;
    let Some(user_len_bytes) = buf.get(p..p + 4) else {
        return EnvelopeParse::Bare;
    };
    let Ok(user_len_arr) = user_len_bytes.try_into() else {
        return EnvelopeParse::Bare;
    };
    let user_len = u32::from_le_bytes(user_len_arr) as usize;
    p += 4;
    if p + user_len > buf.len() {
        return EnvelopeParse::Bare;
    }
    let users = &buf[p..p + user_len];
    p += user_len;
    let publications = if matches!(
        version,
        ENVELOPE_VERSION_V3 | ENVELOPE_VERSION_V4 | ENVELOPE_VERSION_V5
    ) {
        // [u32 pubs_len][publications bytes]
        let Some(pubs_len_bytes) = buf.get(p..p + 4) else {
            return EnvelopeParse::Bare;
        };
        let Ok(pubs_len_arr) = pubs_len_bytes.try_into() else {
            return EnvelopeParse::Bare;
        };
        let pubs_len = u32::from_le_bytes(pubs_len_arr) as usize;
        p += 4;
        if p + pubs_len > buf.len() {
            return EnvelopeParse::Bare;
        }
        let pubs_slice = &buf[p..p + pubs_len];
        p += pubs_len;
        Some(pubs_slice)
    } else {
        None
    };
    let subscriptions = if matches!(version, ENVELOPE_VERSION_V4 | ENVELOPE_VERSION_V5) {
        // [u32 subs_len][subscriptions bytes]
        let Some(subs_len_bytes) = buf.get(p..p + 4) else {
            return EnvelopeParse::Bare;
        };
        let Ok(subs_len_arr) = subs_len_bytes.try_into() else {
            return EnvelopeParse::Bare;
        };
        let subs_len = u32::from_le_bytes(subs_len_arr) as usize;
        p += 4;
        if p + subs_len > buf.len() {
            return EnvelopeParse::Bare;
        }
        let subs_slice = &buf[p..p + subs_len];
        p += subs_len;
        Some(subs_slice)
    } else {
        None
    };
    let statistics = if version == ENVELOPE_VERSION_V5 {
        // [u32 stats_len][statistics bytes]
        let Some(stats_len_bytes) = buf.get(p..p + 4) else {
            return EnvelopeParse::Bare;
        };
        let Ok(stats_len_arr) = stats_len_bytes.try_into() else {
            return EnvelopeParse::Bare;
        };
        let stats_len = u32::from_le_bytes(stats_len_arr) as usize;
        p += 4;
        if p + stats_len > buf.len() {
            return EnvelopeParse::Bare;
        }
        let stats_slice = &buf[p..p + stats_len];
        p += stats_len;
        Some(stats_slice)
    } else {
        None
    };
    if matches!(
        version,
        ENVELOPE_VERSION_V2 | ENVELOPE_VERSION_V3 | ENVELOPE_VERSION_V4 | ENVELOPE_VERSION_V5
    ) {
        if p + 4 != buf.len() {
            return EnvelopeParse::Bare;
        }
        let Ok(crc_arr) = buf[p..p + 4].try_into() else {
            return EnvelopeParse::Bare;
        };
        let expected = u32::from_le_bytes(crc_arr);
        let computed = spg_crypto::crc32::crc32(&buf[..p]);
        if expected != computed {
            return EnvelopeParse::CrcMismatch { expected, computed };
        }
    } else if p != buf.len() {
        // v1: must end exactly at the users section.
        return EnvelopeParse::Bare;
    }
    EnvelopeParse::Pair {
        catalog,
        users,
        publications,
        subscriptions,
        statistics,
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
    /// v4.1 RBAC user table. Empty means "no RBAC configured yet" —
    /// the server decides what that means at the auth boundary
    /// (open mode vs legacy single-password mode). User CRUD goes
    /// through `create_user`/`drop_user`/`verify_user`; persistence
    /// rides the snapshot envelope alongside the catalog.
    users: UserStore,
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
    session_params: BTreeMap<String, String>,
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

/// v6.5.4 — synthesise a `CREATE TABLE` statement from catalog
/// state. Round-trips through `Engine::execute` to recreate the
/// same schema (sans data + indexes — indexes are emitted as a
/// separate `CREATE INDEX` chain in `spg_database_ddl`).
fn render_create_table(name: &str, columns: &[ColumnSchema]) -> String {
    let mut out = alloc::format!("CREATE TABLE {name} (");
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&col.name);
        out.push(' ');
        out.push_str(&render_data_type(col.ty));
        if !col.nullable {
            out.push_str(" NOT NULL");
        }
        if col.auto_increment {
            out.push_str(" AUTO_INCREMENT");
        }
    }
    out.push(')');
    out
}

fn render_data_type(ty: DataType) -> String {
    match ty {
        DataType::SmallInt => "SMALLINT".into(),
        DataType::Int => "INT".into(),
        DataType::BigInt => "BIGINT".into(),
        DataType::Float => "FLOAT".into(),
        DataType::Text => "TEXT".into(),
        DataType::Varchar(n) => alloc::format!("VARCHAR({n})"),
        DataType::Char(n) => alloc::format!("CHAR({n})"),
        DataType::Bool => "BOOL".into(),
        DataType::Vector { dim, encoding } => match encoding {
            spg_storage::VecEncoding::F32 => alloc::format!("VECTOR({dim})"),
            spg_storage::VecEncoding::Sq8 => alloc::format!("VECTOR({dim}) USING SQ8"),
            spg_storage::VecEncoding::F16 => alloc::format!("VECTOR({dim}) USING HALF"),
        },
        DataType::Numeric { precision, scale } => {
            alloc::format!("NUMERIC({precision},{scale})")
        }
        DataType::Date => "DATE".into(),
        DataType::Timestamp => "TIMESTAMP".into(),
        DataType::Interval => "INTERVAL".into(),
        DataType::Json => "JSON".into(),
        DataType::Jsonb => "JSONB".into(),
        DataType::Timestamptz => "TIMESTAMPTZ".into(),
        DataType::Bytes => "BYTEA".into(),
        DataType::TextArray => "TEXT[]".into(),
        DataType::IntArray => "INT[]".into(),
        DataType::BigIntArray => "BIGINT[]".into(),
        DataType::TsVector => "TSVECTOR".into(),
        DataType::TsQuery => "TSQUERY".into(),
        DataType::Uuid => "UUID".into(),
        DataType::Time => "TIME".into(),
        DataType::Year => "YEAR".into(),
        DataType::TimeTz => "TIMETZ".into(),
        DataType::Money => "MONEY".into(),
        DataType::Range(k) => k.keyword().into(),
        DataType::Hstore => "HSTORE".into(),
        DataType::IntArray2D => "INT[][]".into(),
        DataType::BigIntArray2D => "BIGINT[][]".into(),
        DataType::TextArray2D => "TEXT[][]".into(),
    }
}

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

    /// `salt` is supplied by the caller (the host has a random
    /// source; the engine is `no_std`). Caller should pass a fresh
    /// 16-byte random value per user.
    pub fn create_user(
        &mut self,
        name: &str,
        password: &str,
        role: Role,
        salt: [u8; 16],
    ) -> Result<(), UserError> {
        self.users.create(name, password, role, salt)?;
        // v4.8: also derive SCRAM-SHA-256 secrets so PG-wire SASL
        // auth can verify without re-running PBKDF2 per attempt.
        // Uses a fresh salt from the host RNG (falls back to a
        // deterministic per-username salt when no RNG is wired, same
        // as the legacy hash path).
        let scram_salt = self.salt_fn.map_or_else(
            || {
                let mut s = [0u8; users::SCRAM_SALT_LEN];
                let digest = spg_crypto::hash(name.as_bytes());
                // Use bytes 16..32 of BLAKE3 so we don't reuse the
                // exact same fallback salt as the BLAKE3 hash path.
                s.copy_from_slice(&digest[16..32]);
                s
            },
            |f| f(),
        );
        self.users
            .enable_scram(name, password, scram_salt, users::SCRAM_DEFAULT_ITERS)?;
        Ok(())
    }

    pub fn drop_user(&mut self, name: &str) -> Result<(), UserError> {
        self.users.drop(name)
    }

    pub fn verify_user(&self, name: &str, password: &str) -> Option<Role> {
        self.users.verify(name, password)
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

    /// v6.7.3 — public shim around `Catalog::compact_cold_segments`
    /// driving every BTree index on every user table. Returns one
    /// `(table, index, report)` triple for each merge that
    /// actually happened (no-op (table, index) pairs are filtered
    /// out so callers can size persist-side work to the live
    /// merges). Caller is responsible for persisting each
    /// `report.merged_segment_bytes` and updating the on-disk
    /// segment registry; engine layer is no_std and never
    /// touches disk.
    ///
    /// Marks every touched table's cached `cold_row_count` stale
    /// — compaction GC'd some shadowed rows, so the count must be
    /// re-derived on the next ANALYZE.
    pub fn compact_cold_segments_with_target(
        &mut self,
        target_segment_bytes: u64,
    ) -> Result<Vec<(String, String, CompactReport)>, EngineError> {
        let table_names = self.active_catalog().table_names();
        let mut reports: Vec<(String, String, CompactReport)> = Vec::new();
        for tname in table_names {
            if is_internal_table_name(&tname) {
                continue;
            }
            let idx_names: Vec<String> = {
                let Some(t) = self.active_catalog().get(&tname) else {
                    continue;
                };
                t.indices()
                    .iter()
                    .filter(|i| matches!(i.kind, IndexKind::BTree(_)))
                    .map(|i| i.name.clone())
                    .collect()
            };
            for iname in idx_names {
                let report = self
                    .active_catalog_mut()
                    .compact_cold_segments(&tname, &iname, target_segment_bytes)
                    .map_err(EngineError::Storage)?;
                if report.merged_segment_id.is_some() {
                    if let Some(t) = self.active_catalog_mut().get_mut(&tname) {
                        t.mark_cold_row_count_stale();
                    }
                    reports.push((tname.clone(), iname, report));
                }
            }
        }
        Ok(reports)
    }

    fn active_catalog(&self) -> &Catalog {
        match self.current_tx {
            Some(t) => self
                .tx_catalogs
                .get(&t)
                .map_or(&self.catalog, |s| &s.catalog),
            None => &self.catalog,
        }
    }

    /// v7.12.4 — snapshot every row-level trigger on `table` that
    /// fires for `event` (`"INSERT"` / `"UPDATE"` / `"DELETE"`) at
    /// the given `timing` (`"BEFORE"` / `"AFTER"`), and clone its
    /// referenced function definition. Returned as a vec of owned
    /// `FunctionDef` so the row-write loop can fire them without
    /// holding a borrow on the catalog (which would conflict with
    /// the table.insert / update_row / delete mutable borrows).
    /// v7.16.2 — top-level DO block executor. Walks the
    /// PlPgSqlBlock via [`triggers::execute_do_block_top_level`],
    /// then runs each collected EmbeddedSql statement through
    /// the engine's regular execute path (NOT deferred — DO is
    /// outside any row-write borrow). Errors from any step
    /// abort the block and propagate verbatim.
    /// v7.16.2 — resolve every subquery inside a PlPgSqlBlock's
    /// expression slots so the downstream trigger-flavoured
    /// evaluator (which expects pre-resolved Expr::Literal /
    /// Binary chains) doesn't trip on raw Exists/ScalarSubquery
    /// nodes. Walks IF conditions, Assign values, RAISE args.
    /// EmbeddedSql statements re-enter the engine for execution
    /// later so their subqueries get the normal SELECT-side
    /// resolution.
    fn resolve_plpgsql_block_subqueries(
        &self,
        block: &mut spg_sql::ast::PlPgSqlBlock,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for d in &mut block.declarations {
            if let Some(e) = &mut d.default {
                self.resolve_expr_subqueries(e, cancel)?;
            }
        }
        self.resolve_plpgsql_stmts_subqueries(&mut block.statements, cancel)
    }

    fn resolve_plpgsql_stmts_subqueries(
        &self,
        stmts: &mut [spg_sql::ast::PlPgSqlStmt],
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::PlPgSqlStmt;
        for stmt in stmts {
            match stmt {
                PlPgSqlStmt::Assign { value, .. } => {
                    self.resolve_expr_subqueries(value, cancel)?;
                }
                PlPgSqlStmt::Return(spg_sql::ast::ReturnTarget::Expr(e)) => {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                PlPgSqlStmt::Return(_) => {}
                PlPgSqlStmt::If {
                    branches,
                    else_branch,
                } => {
                    for (cond, body) in branches.iter_mut() {
                        self.resolve_expr_subqueries(cond, cancel)?;
                        self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                    }
                    self.resolve_plpgsql_stmts_subqueries(else_branch, cancel)?;
                }
                PlPgSqlStmt::Raise { args, .. } => {
                    for a in args {
                        self.resolve_expr_subqueries(a, cancel)?;
                    }
                }
                PlPgSqlStmt::EmbeddedSql(_) => {
                    // Embedded SQL goes back through execute_stmt
                    // _with_cancel which runs the SELECT-side
                    // resolver itself; nothing to do here.
                }
                PlPgSqlStmt::SelectInto { body, .. } => {
                    // SELECT INTO runs through Engine::execute
                    // when reached, so subquery resolution
                    // happens via the normal SELECT-side path.
                    // Still walk for nested subqueries inside
                    // the SELECT body so eval doesn't trip.
                    self.resolve_select_subqueries(body, cancel)?;
                }
            }
        }
        Ok(())
    }

    fn exec_do_block(
        &mut self,
        body: spg_sql::ast::PlPgSqlBlock,
    ) -> Result<QueryResult, EngineError> {
        // v7.16.2 — pre-resolve every subquery the body's
        // expressions reach. `eval::eval_expr` errors on
        // unresolved Exists/ScalarSubquery/InSubquery; the
        // top-level SELECT path runs `resolve_select_subqueries`
        // for the caller — for plpgsql we have to do the
        // equivalent before the body walker runs. Catches the
        // mailrs idiom `IF EXISTS (SELECT 1 FROM
        // information_schema.columns WHERE …) THEN …`.
        let mut body = body;
        self.resolve_plpgsql_block_subqueries(&mut body, CancelToken::none())?;
        let dts = self
            .session_param("default_text_search_config")
            .map(String::from);
        // v7.16.2 — SELECT … INTO resolver. The walker calls
        // this synchronously when it hits a SelectInto stmt
        // so the IF / locals scope sees the result before the
        // next statement. Body walks for trigger paths (no
        // resolver) error loudly on SelectInto.
        // SAFETY: the closure shares this engine borrow with
        // the walker, but the walker only borrows for the
        // duration of `execute_do_block_top_level` and doesn't
        // reach back into the engine through any other path —
        // so the recursive `&mut` is sound. We use a `RefCell`
        // for interior mutability since the closure is
        // Fn-shaped.
        let engine_cell = core::cell::RefCell::new(&mut *self);
        let resolver_fn =
            |stmt: &spg_sql::ast::Statement| -> Result<Value, triggers::TriggerError> {
                let mut eng = engine_cell.borrow_mut();
                let r = eng
                    .execute_stmt_with_cancel(stmt.clone(), CancelToken::none())
                    .map_err(|e| triggers::TriggerError::EvalFailed {
                        function: "DO".into(),
                        cause: eval::EvalError::TypeMismatch {
                            detail: alloc::format!("SELECT … INTO failed: {e}"),
                        },
                    })?;
                match r {
                    QueryResult::Rows { rows, .. } => match rows.into_iter().next() {
                        Some(row) => Ok(row.values.into_iter().next().unwrap_or(Value::Null)),
                        None => Ok(Value::Null),
                    },
                    _ => Err(triggers::TriggerError::EvalFailed {
                        function: "DO".into(),
                        cause: eval::EvalError::TypeMismatch {
                            detail: "SELECT … INTO body must be a SELECT".into(),
                        },
                    }),
                }
            };
        let collected =
            triggers::execute_do_block_top_level(&body, dts.as_deref(), Some(&resolver_fn))
                .map_err(|e| {
                    EngineError::Storage(StorageError::Corrupt(alloc::format!("DO: {e}")))
                })?;
        // engine_cell goes out of scope here, releasing the &mut self borrow
        // Run each embedded statement against the engine. The
        // statements were already substitute-walked for NEW/OLD/
        // locals (those evaluate to engine literals before they
        // land here) so dispatch is plain execute_stmt_with_cancel.
        for stmt in collected {
            // v7.16.2 — preserve current_tx wrap so an outer
            // BEGIN/COMMIT around a DO block keeps the
            // EmbeddedSql writes inside that same tx slot.
            self.execute_stmt_with_cancel(stmt, CancelToken::none())?;
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn snapshot_row_triggers(
        &self,
        table: &str,
        event: &str,
        timing: &str,
    ) -> Vec<spg_storage::FunctionDef> {
        let cat = self.active_catalog();
        cat.triggers()
            .iter()
            .filter(|t| {
                // v7.16.1 — skip disabled triggers (mailrs
                // round-9 A.2.b — pg_dump --disable-triggers).
                t.enabled
                    && t.table == table
                    && t.timing.eq_ignore_ascii_case(timing)
                    && t.for_each.eq_ignore_ascii_case("row")
                    && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
            })
            .filter_map(|t| cat.functions().get(&t.function).cloned())
            .collect()
    }

    /// v7.13.0 — UPDATE-side snapshot that pairs each trigger's
    /// function with its `UPDATE OF cols` filter (mailrs round-5
    /// G7). Empty filter Vec means "fire unconditionally", matching
    /// the v7.12 behaviour.
    fn snapshot_update_row_triggers(
        &self,
        table: &str,
        timing: &str,
    ) -> Vec<(spg_storage::FunctionDef, Vec<String>)> {
        let cat = self.active_catalog();
        cat.triggers()
            .iter()
            .filter(|t| {
                // v7.16.1 — skip disabled triggers.
                t.enabled
                    && t.table == table
                    && t.timing.eq_ignore_ascii_case(timing)
                    && t.for_each.eq_ignore_ascii_case("row")
                    && t.events.iter().any(|e| e.eq_ignore_ascii_case("UPDATE"))
            })
            .filter_map(|t| {
                cat.functions()
                    .get(&t.function)
                    .cloned()
                    .map(|fd| (fd, t.update_columns.clone()))
            })
            .collect()
    }

    /// v7.12.7 — drain the trigger-emitted embedded SQL queue.
    /// Called by the INSERT / UPDATE / DELETE executors after
    /// their main row-write loop returns. Each statement runs
    /// inside the same cancel scope as the firing DML and bumps
    /// the recursion counter; nested embedded SQL beyond
    /// [`MAX_TRIGGER_RECURSION`] errors with a clear message so
    /// a trigger-graph cycle surfaces as a query failure instead
    /// of stack-blowing the engine.
    fn execute_deferred_trigger_stmts(
        &mut self,
        deferred: Vec<triggers::DeferredEmbeddedStmt>,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for d in deferred {
            if self.trigger_recursion_depth >= MAX_TRIGGER_RECURSION {
                return Err(EngineError::Storage(StorageError::Corrupt(alloc::format!(
                    "trigger embedded SQL recursion depth {} exceeded (trigger function \
                     {:?} would push past the {} cap — check for trigger cycles)",
                    self.trigger_recursion_depth,
                    d.function,
                    MAX_TRIGGER_RECURSION,
                ))));
            }
            self.trigger_recursion_depth += 1;
            let res = self.execute_stmt_with_cancel(d.stmt, cancel);
            self.trigger_recursion_depth -= 1;
            res?;
        }
        Ok(())
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
    fn enforce_row_limit(
        &self,
        result: Result<QueryResult, EngineError>,
    ) -> Result<QueryResult, EngineError> {
        if let (Ok(QueryResult::Rows { rows, .. }), Some(cap)) = (&result, self.max_query_rows)
            && rows.len() > cap
        {
            return Err(EngineError::RowLimitExceeded(cap));
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

    /// v6.1.2 — `CREATE PUBLICATION` runtime path. Duplicate names
    /// surface as `EngineError::Unsupported` so the existing PG-wire
    /// error mapping stays uniform; the message carries the name so
    /// operators can grep replication-log noise. Inside-transaction
    /// invocation is rejected (matches `CREATE USER` / `DROP USER`
    /// stance) — replication-catalog mutation is a connection-level
    /// administrative op, not a transactional one.
    fn exec_create_publication(
        &mut self,
        s: CreatePublicationStatement,
    ) -> Result<QueryResult, EngineError> {
        // v6.1.4 — the v6.1.2 "no DDL inside a transaction" guard
        // was over-cautious: it also blocked the auto-commit wrap
        // path (which begins an internal TX around every WAL-
        // logged statement). PG itself allows CREATE PUBLICATION
        // inside a transaction (it rolls back with the TX).
        self.publications
            .create(s.name, s.scope)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE PUBLICATION: {e:?}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    /// v6.1.2 — `DROP PUBLICATION` runtime path. PG-compatible silent
    /// no-op when the publication doesn't exist (returns `affected=0`
    /// in that case so the wire-level command tag distinguishes
    /// "dropped" from "no-op", though both succeed).
    fn exec_drop_publication(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        let removed = self.publications.drop(name);
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    /// v6.1.2 — read access to the publication catalog. Used by
    /// the v6.1.5 publisher-side WAL filter, by `SHOW PUBLICATIONS`
    /// (v6.1.3+), and by e2e tests that need to assert state without
    /// going through the wire.
    pub const fn publications(&self) -> &publications::Publications {
        &self.publications
    }

    /// v6.1.4 — `CREATE SUBSCRIPTION` runtime path. Defaults
    /// `enabled = true` and `last_received_pos = 0` for a freshly-
    /// created subscription. The actual worker thread is spawned
    /// by spg-server once the engine returns success.
    fn exec_create_subscription(
        &mut self,
        s: CreateSubscriptionStatement,
    ) -> Result<QueryResult, EngineError> {
        // See exec_create_publication — the in_transaction gate
        // was over-cautious; the auto-commit wrap path holds an
        // internal TX that this check was incorrectly blocking.
        let sub = subscriptions::Subscription {
            conn_str: s.conn_str,
            publications: s.publications,
            enabled: true,
            last_received_pos: 0,
        };
        self.subscriptions
            .create(s.name, sub)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE SUBSCRIPTION: {e:?}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    /// v6.1.4 — `DROP SUBSCRIPTION`. Silent no-op when the name
    /// doesn't exist (PG-compatible). The associated worker is
    /// torn down by spg-server when it observes the catalog
    /// change at the next snapshot or via the engine's
    /// subscriptions accessor (the worker polls the catalog on
    /// reconnect; v6.1.5's filter-side will tighten this to an
    /// explicit signal).
    fn exec_drop_subscription(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        let removed = self.subscriptions.drop(name);
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    /// v6.1.4 — read access to the subscription catalog. Used by
    /// the subscription worker (read its own row to find its
    /// publications + last applied position), by SHOW SUBSCRIPTIONS,
    /// and by e2e tests asserting state directly.
    pub const fn subscriptions(&self) -> &subscriptions::Subscriptions {
        &self.subscriptions
    }

    /// v6.1.4 — write access to `last_received_pos`. Worker
    /// calls this after each apply batch (under the engine's
    /// write-lock). Returns `false` when the subscription was
    /// dropped between when the worker received the record and
    /// when this call landed.
    pub fn subscription_advance(&mut self, name: &str, pos: u64) -> bool {
        self.subscriptions.update_last_received_pos(name, pos)
    }

    /// v6.1.4 — `SHOW SUBSCRIPTIONS` row materialisation. Returns
    /// `(name, conn_str, publications, enabled, last_received_pos)`
    /// ordered by subscription name. The `publications` column is
    /// the comma-joined list ("p1, p2") for ergonomic SHOW output;
    /// callers wanting structured access read `Engine::subscriptions`.
    fn exec_show_subscriptions(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("conn_str", DataType::Text, false),
            ColumnSchema::new("publications", DataType::Text, false),
            ColumnSchema::new("enabled", DataType::Bool, false),
            ColumnSchema::new("last_received_pos", DataType::BigInt, false),
        ];
        let rows: Vec<Row> = self
            .subscriptions
            .iter()
            .map(|(name, sub)| {
                Row::new(alloc::vec![
                    Value::Text(name.clone()),
                    Value::Text(sub.conn_str.clone()),
                    Value::Text(sub.publications.join(", ")),
                    Value::Bool(sub.enabled),
                    Value::BigInt(i64::try_from(sub.last_received_pos).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.2.0 — materialise `spg_statistic` rows. One row per
    /// `(table, column)` pair tracked in `Statistics`, with
    /// `histogram_bounds` rendered as a `[v0, v1, ...]` string —
    /// the same canonical form vector literals use for round-trip.
    fn exec_spg_statistic(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("column_name", DataType::Text, false),
            ColumnSchema::new("null_frac", DataType::Float, false),
            ColumnSchema::new("n_distinct", DataType::BigInt, false),
            ColumnSchema::new("histogram_bounds", DataType::Text, false),
            // v6.7.0 — appended column (v6.2.0 stability contract
            // allows APPEND to spg_statistic, not reorder/rename).
            // Reports the cached per-table cold-row count; same
            // value across every column row of the same table.
            ColumnSchema::new("cold_row_count", DataType::BigInt, false),
        ];
        let rows: Vec<Row> = self
            .statistics
            .iter()
            .map(|((t, c), s)| {
                let cold = self
                    .catalog
                    .get(t)
                    .map_or(0, |table| table.cold_row_count());
                Row::new(alloc::vec![
                    Value::Text(t.clone()),
                    Value::Text(c.clone()),
                    Value::Float(f64::from(s.null_frac)),
                    Value::BigInt(i64::try_from(s.n_distinct).unwrap_or(i64::MAX)),
                    Value::Text(render_histogram_bounds(&s.histogram_bounds)),
                    Value::BigInt(i64::try_from(cold).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.0 — materialise `spg_stat_replication` rows. One row
    /// per subscription with `(name, conn_str, publications,
    /// last_received_pos, enabled)`. Surface mirrors
    /// `SHOW SUBSCRIPTIONS` but follows the virtual-table dispatch
    /// shape so it composes with SELECT clauses (WHERE, projection
    /// onto specific columns, etc).
    fn exec_spg_stat_replication(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("conn_str", DataType::Text, false),
            ColumnSchema::new("publications", DataType::Text, false),
            ColumnSchema::new("last_received_pos", DataType::BigInt, false),
            ColumnSchema::new("enabled", DataType::Bool, false),
        ];
        let rows: Vec<Row> = self
            .subscriptions
            .iter()
            .map(|(name, sub)| {
                Row::new(alloc::vec![
                    Value::Text(name.clone()),
                    Value::Text(sub.conn_str.clone()),
                    Value::Text(sub.publications.join(",")),
                    Value::BigInt(i64::try_from(sub.last_received_pos).unwrap_or(i64::MAX)),
                    Value::Bool(sub.enabled),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.0 — materialise `spg_stat_segment` rows. One row per
    /// cold-tier segment with `(segment_id, num_rows, num_pages,
    /// total_bytes)`.
    ///
    /// v6.7.0 — appended `table_name` column resolves the v6.5.0
    /// carve-out. Walks every user table's BTree indices to find
    /// which table's Cold locators point at each segment. Empty
    /// string for orphan segments (loaded via SPG_PRELOAD_COLD_SEGMENT
    /// before any index registered a locator). The walk is
    /// O(tables × indices × keys); cached per call, not across
    /// calls — re-walked on every `SELECT * FROM spg_stat_segment`.
    fn exec_spg_stat_segment(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("segment_id", DataType::BigInt, false),
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("num_rows", DataType::BigInt, false),
            ColumnSchema::new("num_pages", DataType::BigInt, false),
            ColumnSchema::new("total_bytes", DataType::BigInt, false),
        ];
        // v6.7.0 — build a segment_id → table_name map by walking
        // every user table's BTree indices once. O(tables × indices
        // × keys) for the v6.5.0 carve-out resolution; acceptable
        // because spg_stat_segment is operator-facing (not on a
        // hot-loop path).
        let mut segment_owners: alloc::collections::BTreeMap<u32, String> = BTreeMap::new();
        for tname in self.catalog.table_names() {
            if is_internal_table_name(&tname) {
                continue;
            }
            let Some(t) = self.catalog.get(&tname) else {
                continue;
            };
            for idx in t.indices() {
                if let spg_storage::IndexKind::BTree(map) = &idx.kind {
                    for (_, locs) in map.iter() {
                        for loc in locs {
                            if let spg_storage::RowLocator::Cold { segment_id, .. } = loc {
                                segment_owners
                                    .entry(*segment_id)
                                    .or_insert_with(|| tname.clone());
                            }
                        }
                    }
                }
            }
        }
        let rows: Vec<Row> = self
            .catalog
            .cold_segment_ids_global()
            .iter()
            .filter_map(|&id| {
                let seg = self.catalog.cold_segment(id)?;
                let meta = seg.meta();
                let owner = segment_owners.get(&id).cloned().unwrap_or_default();
                Some(Row::new(alloc::vec![
                    Value::BigInt(i64::from(id)),
                    Value::Text(owner),
                    Value::BigInt(i64::try_from(meta.num_rows).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::from(meta.num_pages)),
                    Value::BigInt(i64::try_from(meta.total_bytes).unwrap_or(i64::MAX)),
                ]))
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.1 — materialise `spg_stat_query` rows. One row per
    /// distinct SQL text recorded since the engine booted, capped
    /// at `QUERY_STATS_MAX` (1024). Columns:
    ///   sql, exec_count, total_us, mean_us, max_us, last_seen_us
    /// mean_us = total_us / exec_count (saturating).
    fn exec_spg_stat_query(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("sql", DataType::Text, false),
            ColumnSchema::new("exec_count", DataType::BigInt, false),
            ColumnSchema::new("total_us", DataType::BigInt, false),
            ColumnSchema::new("mean_us", DataType::BigInt, false),
            ColumnSchema::new("max_us", DataType::BigInt, false),
            ColumnSchema::new("last_seen_us", DataType::BigInt, false),
        ];
        let rows: Vec<Row> = self
            .query_stats
            .snapshot()
            .into_iter()
            .map(|(sql, s)| {
                let mean = if s.exec_count == 0 {
                    0
                } else {
                    s.total_us / s.exec_count
                };
                Row::new(alloc::vec![
                    Value::Text(sql),
                    Value::BigInt(i64::try_from(s.exec_count).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.total_us).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(mean).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.max_us).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.last_seen_us).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.2 — register a connection-state provider. spg-server
    /// calls this at startup with a function that snapshots its
    /// per-pgwire-connection registry. Engine reads through the
    /// callback on `SELECT * FROM spg_stat_activity`.
    #[must_use]
    pub const fn with_activity_provider(mut self, f: ActivityProvider) -> Self {
        self.activity_provider = Some(f);
        self
    }

    /// v6.5.3 — register audit chain provider + verifier.
    #[must_use]
    pub const fn with_audit_providers(
        mut self,
        chain: AuditChainProvider,
        verify: AuditVerifier,
    ) -> Self {
        self.audit_chain_provider = Some(chain);
        self.audit_verifier = Some(verify);
        self
    }

    /// v6.5.6 — register a slow-query log callback. `threshold_us`
    /// is the floor (in microseconds); only executes above the floor
    /// fire the callback. spg-server wires this from
    /// `SPG_SLOW_QUERY_THRESHOLD_MS` (default 100 ms).
    #[must_use]
    pub const fn with_slow_query_log(mut self, threshold_us: u64, logger: SlowQueryLogger) -> Self {
        self.slow_query_threshold_us = Some(threshold_us);
        self.slow_query_logger = Some(logger);
        self
    }

    /// v6.5.6 — operator knob for plan cache cap. spg-server reads
    /// `SPG_PLAN_CACHE_MAX` env at startup; uses this to override
    /// the compile-time default of 256.
    pub fn set_plan_cache_max(&mut self, n: usize) {
        self.plan_cache.set_max_entries(n);
    }

    /// v6.5.2 — materialise `spg_stat_activity` rows. Pulls a fresh
    /// snapshot from the registered `ActivityProvider`. Returns an
    /// empty result set when no provider is registered (the no_std
    /// embedded path with no pgwire layer).
    fn exec_spg_stat_activity(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("pid", DataType::Int, false),
            ColumnSchema::new("user", DataType::Text, false),
            ColumnSchema::new("started_at_us", DataType::BigInt, false),
            ColumnSchema::new("current_sql", DataType::Text, false),
            ColumnSchema::new("wait_event", DataType::Text, false),
            ColumnSchema::new("elapsed_us", DataType::BigInt, false),
            ColumnSchema::new("in_transaction", DataType::Bool, false),
            ColumnSchema::new("application_name", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .activity_provider
            .map(|f| f())
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from(r.pid).unwrap_or(i32::MAX)),
                    Value::Text(r.user),
                    Value::BigInt(r.started_at_us),
                    Value::Text(r.current_sql),
                    Value::Text(r.wait_event),
                    Value::BigInt(r.elapsed_us),
                    Value::Bool(r.in_transaction),
                    Value::Text(r.application_name),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_table_ddl` rows. One row per user
    /// table with `(table_name, ddl)`. Reconstructed from catalog
    /// state on demand.
    fn exec_spg_table_ddl(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("ddl", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .catalog
            .table_names()
            .into_iter()
            .filter(|n| !is_internal_table_name(n))
            .filter_map(|name| {
                let table = self.catalog.get(&name)?;
                let ddl = render_create_table(&name, &table.schema().columns);
                Some(Row::new(alloc::vec![Value::Text(name), Value::Text(ddl),]))
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_role_ddl` rows. One row per user
    /// with `(role_name, ddl)`. Password is redacted (matches the
    /// `Statement::CreateUser` Display which prints `'<redacted>'`).
    fn exec_spg_role_ddl(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("role_name", DataType::Text, false),
            ColumnSchema::new("ddl", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .users
            .iter()
            .map(|(name, rec)| {
                let ddl = alloc::format!(
                    "CREATE USER {name} WITH PASSWORD '<redacted>' ROLE '{}'",
                    rec.role.as_str(),
                );
                Row::new(alloc::vec![
                    Value::Text(String::from(name)),
                    Value::Text(ddl)
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_database_ddl`: single row whose
    /// `ddl` column concatenates every user table's CREATE +
    /// every role's CREATE in deterministic catalog order. Suitable
    /// for piping back through `Engine::execute` to recreate a
    /// schema-equivalent database.
    fn exec_spg_database_ddl(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("ddl", DataType::Text, false)];
        let mut out = String::new();
        for (name, rec) in self.users.iter() {
            out.push_str(&alloc::format!(
                "CREATE USER {name} WITH PASSWORD '<redacted>' ROLE '{}';\n",
                rec.role.as_str(),
            ));
        }
        for name in self.catalog.table_names() {
            if is_internal_table_name(&name) {
                continue;
            }
            if let Some(table) = self.catalog.get(&name) {
                out.push_str(&render_create_table(&name, &table.schema().columns));
                out.push_str(";\n");
            }
        }
        QueryResult::Rows {
            columns,
            rows: alloc::vec![Row::new(alloc::vec![Value::Text(out)])],
        }
    }

    /// v6.5.3 — materialise `spg_audit_chain` rows. Pulls a fresh
    /// snapshot from the registered provider; empty when no
    /// provider is set.
    fn exec_spg_audit_chain(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("seq", DataType::BigInt, false),
            ColumnSchema::new("ts_ms", DataType::BigInt, false),
            ColumnSchema::new("prev_hash", DataType::Text, false),
            ColumnSchema::new("entry_hash", DataType::Text, false),
            ColumnSchema::new("sql", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .audit_chain_provider
            .map(|f| f())
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                Row::new(alloc::vec![
                    Value::BigInt(r.seq),
                    Value::BigInt(r.ts_ms),
                    Value::Text(r.prev_hash_hex),
                    Value::Text(r.entry_hash_hex),
                    Value::Text(r.sql),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.3 — materialise `spg_audit_verify` single-row result.
    /// `(verified_count, broken_at_seq)` — broken_at_seq is `-1`
    /// on a clean chain. Returns one row with both values 0 when
    /// no verifier is registered (no-data fallback for embedded
    /// callers).
    fn exec_spg_audit_verify(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("verified_count", DataType::BigInt, false),
            ColumnSchema::new("broken_at_seq", DataType::BigInt, false),
        ];
        let (verified, broken) = self.audit_verifier.map(|f| f()).unwrap_or((0, -1));
        let row = Row::new(alloc::vec![Value::BigInt(verified), Value::BigInt(broken),]);
        QueryResult::Rows {
            columns,
            rows: alloc::vec![row],
        }
    }

    /// v6.5.1 — read-only accessor for tests + v6.5.6 ops resets.
    pub fn query_stats(&self) -> &query_stats::QueryStats {
        &self.query_stats
    }

    /// v6.5.1 — mutable accessor (clear, etc).
    pub fn query_stats_mut(&mut self) -> &mut query_stats::QueryStats {
        &mut self.query_stats
    }

    /// v6.2.0 — read access to the per-column statistics table.
    /// Used by the planner (v6.2.2 selectivity functions read this),
    /// by `SELECT * FROM spg_statistic`, and by e2e tests.
    pub const fn statistics(&self) -> &statistics::Statistics {
        &self.statistics
    }

    /// v6.2.1 — return tables whose modified-row count crossed the
    /// auto-analyze threshold since the last ANALYZE on that table.
    /// The threshold is `0.1 × max(row_count, MIN_ROWS_FOR_AUTO_
    /// ANALYZE)` — combines PG-style fractional + absolute lower
    /// bound so a fresh / tiny table doesn't get hammered on every
    /// INSERT.
    ///
    /// Designed to be cheap: walks every user table's
    /// `Catalog::table_names()` + reads `statistics::modified_
    /// since_last_analyze()` (BTreeMap lookup). The background
    /// worker calls this under `engine.read()` then drops the lock
    /// before re-acquiring `engine.write()` for the actual ANALYZE.
    pub fn tables_needing_analyze(&self) -> Vec<String> {
        const MIN_ROWS: u64 = 100;
        let mut out = Vec::new();
        for name in self.catalog.table_names() {
            if is_internal_table_name(&name) {
                continue;
            }
            let Some(table) = self.catalog.get(&name) else {
                continue;
            };
            let row_count = table.rows().len() as u64;
            let modified = self.statistics.modified_since_last_analyze(&name);
            // Threshold: ceil(0.1 × max(row_count, MIN_ROWS)),
            // computed in integer arithmetic so spg-engine stays
            // no_std without pulling in libm. `(n + 9) / 10` is
            // `ceil(n / 10)` for non-negative `n`.
            let base = row_count.max(MIN_ROWS);
            let threshold = base.saturating_add(9) / 10;
            if modified >= threshold {
                out.push(name);
            }
        }
        out
    }

    /// v6.2.0 — `ANALYZE [<table>]` runtime. Bare `ANALYZE` walks
    /// every user table; `ANALYZE <name>` re-stats one. For each
    /// target table, single-pass scan + per-column histogram +
    /// `null_frac` + `n_distinct`. Replaces the table's prior
    /// stats; resets the modified-row counter.
    ///
    /// v6.2.0 doesn't sample — it scans the full table. v6.2.x
    /// can add reservoir sampling at the > 100 K-row mark; not a
    /// scope blocker for the current commit since rows ≤ 100 K
    /// analyse in milliseconds.
    fn exec_analyze(&mut self, target: Option<&str>) -> Result<QueryResult, EngineError> {
        let names: Vec<String> = if let Some(name) = target {
            // Verify the table exists; surface a clear error if not.
            if self.catalog.get(name).is_none() {
                return Err(EngineError::Storage(StorageError::TableNotFound {
                    name: name.to_string(),
                }));
            }
            alloc::vec![name.to_string()]
        } else {
            self.catalog
                .table_names()
                .into_iter()
                .filter(|n| !is_internal_table_name(n))
                .collect()
        };
        let mut analysed = 0usize;
        for table_name in &names {
            self.analyze_one_table(table_name)?;
            analysed += 1;
        }
        // v6.3.1 — plan cache invalidation. Bump stats version so
        // future lookups see the new generation, and selectively
        // evict every plan whose `source_tables` overlap with the
        // ANALYZE target set. Bare ANALYZE (all tables) clears the
        // whole cache.
        if analysed > 0 {
            self.statistics.bump_version();
            if target.is_some() {
                for t in &names {
                    self.plan_cache.evict_referencing(t);
                }
            } else {
                self.plan_cache.clear();
            }
        }
        Ok(QueryResult::CommandOk {
            affected: analysed,
            modified_catalog: true,
        })
    }

    /// v6.7.3 — `COMPACT COLD SEGMENTS` runtime path. Drives the
    /// engine-layer compaction shim with the default
    /// 4 MiB segment-size threshold. spg-server intercepts the
    /// SQL before it reaches the engine on a server build —
    /// it reads `SPG_COMPACTION_TARGET_SEGMENT_BYTES`, calls
    /// `Engine::compact_cold_segments_with_target` directly with
    /// the env value, and persists every merged segment to
    /// v7.12.1 — record a `SET <name> = <value>` parameter. Names
    /// are case-folded to lowercase to match PG; values keep their
    /// caller-supplied form so observability paths see what was
    /// requested. Only `default_text_search_config` is consulted by
    /// the engine today.
    fn set_session_param(&mut self, name: String, value: spg_sql::ast::SetValue) {
        let normalised = match value {
            spg_sql::ast::SetValue::String(s) => s,
            spg_sql::ast::SetValue::Ident(s) => s,
            spg_sql::ast::SetValue::Number(s) => s,
            spg_sql::ast::SetValue::Default => String::new(),
        };
        let key = name.to_ascii_lowercase();
        // v7.14.0 — mysqldump preamble emits
        // `SET FOREIGN_KEY_CHECKS=0` so it can CREATE TABLE in any
        // order despite cross-table FK references; the closing
        // section emits `SET FOREIGN_KEY_CHECKS=1` (or
        // `=@OLD_FOREIGN_KEY_CHECKS` which resolves to "ON" in our
        // session-variable-aware path). Match both shapes.
        // Also accept PG's `session_replication_role = 'replica'`
        // which suppresses trigger + FK enforcement during a
        // logical replication apply (pg_dump preserves this for
        // schema-only mode but it shows up in some restores).
        let value_off = matches!(
            normalised.to_ascii_lowercase().as_str(),
            "0" | "off" | "false"
        );
        let value_on = matches!(
            normalised.to_ascii_lowercase().as_str(),
            "1" | "on" | "true"
        );
        if key == "foreign_key_checks"
            || key == "session_replication_role" && normalised.eq_ignore_ascii_case("replica")
        {
            if value_off || key == "session_replication_role" {
                self.foreign_key_checks = false;
            } else if value_on
                || (key == "session_replication_role" && normalised.eq_ignore_ascii_case("origin"))
            {
                self.foreign_key_checks = true;
                // Drain pending FK queue against the now-complete
                // catalog. Errors here surface as the SET reply —
                // caller knows enabling checks revealed orphans.
                let _ = self.drain_pending_foreign_keys();
            }
        }
        // v7.22 (round-13 T3) — string-literal dialect signals.
        // `SET sql_mode = …` is something only MySQL clients and
        // mysqldump preambles emit → MySQL escape semantics.
        // `SET standard_conforming_strings = on|off` is PG's own
        // switch for exactly this behaviour (every pg_dump preamble
        // sets it to on). The same SQL text lexes differently per
        // dialect, so a flip invalidates the plan cache.
        let new_escapes = if key == "sql_mode" {
            Some(true)
        } else if key == "standard_conforming_strings" {
            Some(value_off)
        } else {
            None
        };
        if let Some(flag) = new_escapes
            && flag != self.backslash_escapes
        {
            self.backslash_escapes = flag;
            self.plan_cache.clear();
        }
        self.session_params.insert(key, normalised);
    }

    /// v7.14.0 — resolve every queued FK whose installation was
    /// deferred (`SET FOREIGN_KEY_CHECKS=0` window). Called by
    /// `set_session_param` when checks flip back on and by the
    /// drop-import release gate. Each FK is resolved against the
    /// current catalog; remaining missing-parent errors propagate
    /// up so the caller knows the import was incomplete.
    fn drain_pending_foreign_keys(&mut self) -> Result<(), EngineError> {
        let pending = core::mem::take(&mut self.pending_foreign_keys);
        for (child, fk) in pending {
            // Resolve against the current catalog. Skip silently
            // when the child table itself was dropped between
            // queue + drain.
            let cols_snapshot = match self.active_catalog().get(&child) {
                Some(t) => t.schema().columns.clone(),
                None => continue,
            };
            let storage_fk =
                resolve_foreign_key(&child, &cols_snapshot, fk, self.active_catalog())?;
            let table = self
                .active_catalog_mut()
                .get_mut(&child)
                .expect("checked above");
            table.schema_mut().foreign_keys.push(storage_fk);
        }
        Ok(())
    }

    /// v7.12.1 — read a session parameter set via `SET`. Used by
    /// the FTS function dispatcher to resolve the default config
    /// for `to_tsvector(text)` / `plainto_tsquery(text)` etc.
    #[must_use]
    pub fn session_param(&self, name: &str) -> Option<&str> {
        self.session_params
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// v7.12.1 — build an `EvalContext` chained with the session's
    /// `default_text_search_config`. Engine-internal callers use
    /// this instead of `EvalContext::new` so the FTS function
    /// dispatcher sees the SET configuration.
    fn ev_ctx<'a>(
        &'a self,
        columns: &'a [ColumnSchema],
        alias: Option<&'a str>,
    ) -> EvalContext<'a> {
        EvalContext::new(columns, alias)
            .with_default_text_search_config(self.session_param("default_text_search_config"))
    }

    /// `<db>.spg/segments/`. This arm only fires for engine-only
    /// callers (spg-embedded, lib tests); in that mode merged
    /// segments live in memory and are dropped at process exit.
    fn exec_compact_cold_segments(&mut self) -> Result<QueryResult, EngineError> {
        let target = COMPACTION_TARGET_DEFAULT_BYTES;
        let reports = self.compact_cold_segments_with_target(target)?;
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("index_name", DataType::Text, false),
            ColumnSchema::new("sources_merged", DataType::BigInt, false),
            ColumnSchema::new("merged_segment_id", DataType::BigInt, false),
            ColumnSchema::new("merged_rows", DataType::BigInt, false),
            ColumnSchema::new("deleted_rows_pruned", DataType::BigInt, false),
            ColumnSchema::new("bytes_reclaimed_estimate", DataType::BigInt, false),
        ];
        let rows: Vec<Row> = reports
            .into_iter()
            .map(|(tname, iname, report)| {
                Row::new(alloc::vec![
                    Value::Text(tname),
                    Value::Text(iname),
                    Value::BigInt(i64::try_from(report.sources.len()).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::from(report.merged_segment_id.unwrap_or(0))),
                    Value::BigInt(i64::try_from(report.merged_rows).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(report.deleted_rows_pruned).unwrap_or(i64::MAX),),
                    Value::BigInt(
                        i64::try_from(report.bytes_reclaimed_estimate).unwrap_or(i64::MAX),
                    ),
                ])
            })
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

    /// Walk a single table's rows once and (re-)populate per-column
    /// stats. Drops the existing stats for `table` first so columns
    /// that have been DROP-ed between ANALYZEs don't leave stale
    /// rows.
    fn analyze_one_table(&mut self, table_name: &str) -> Result<(), EngineError> {
        let table = self.catalog.get(table_name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: table_name.to_string(),
            })
        })?;
        let schema = table.schema().clone();
        let row_count = table.rows().len();
        // For each column, collect (sorted) non-NULL textual values
        // + count NULLs; then ask `statistics::build_histogram` to
        // produce the 101 bounds and `estimate_n_distinct` the
        // distinct count.
        self.statistics.clear_table(table_name);
        for (col_pos, col_schema) in schema.columns.iter().enumerate() {
            // v6.2.0 skip: vector columns have their own stats
            // shape (HNSW graph topology). v6.2 deliberation #1.
            if matches!(col_schema.ty, DataType::Vector { .. }) {
                continue;
            }
            let mut non_null_values: Vec<Value> = Vec::with_capacity(row_count);
            let mut nulls: u64 = 0;
            for row in table.rows() {
                match row.values.get(col_pos) {
                    Some(Value::Null) | None => nulls += 1,
                    Some(v) => non_null_values.push(v.clone()),
                }
            }
            // Sort by type-aware ordering (Int as int, Text as
            // lex, etc.) so histogram bounds reflect the column's
            // natural order — not lexicographic on the string
            // representation, which would put "9" after "49".
            non_null_values.sort_by(|a, b| sort_values_for_histogram(a, b));
            let non_null: Vec<String> = non_null_values.iter().map(canonical_value_repr).collect();
            let null_frac = if row_count == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let f = nulls as f32 / row_count as f32;
                f
            };
            let n_distinct = statistics::estimate_n_distinct(&non_null);
            let histogram_bounds = statistics::build_histogram(&non_null);
            self.statistics.set(
                table_name.to_string(),
                col_schema.name.clone(),
                statistics::ColumnStats {
                    null_frac,
                    n_distinct,
                    histogram_bounds,
                },
            );
        }
        self.statistics.reset_modified(table_name);
        // v6.7.0 — refresh the per-table cold_rows cache. Walk the
        // BTree indices and count Cold locators (MAX across
        // indices); store the result on the table. Surfaced via
        // `spg_statistic.cold_row_count` (new column) and
        // `spg_stat_segment.table_name` (new column).
        let cold_count = {
            let table = self
                .active_catalog()
                .get(table_name)
                .expect("table still present");
            table.count_cold_locators()
        };
        let table_mut = self
            .active_catalog_mut()
            .get_mut(table_name)
            .expect("table still present");
        table_mut.set_cold_row_count(cold_count);
        Ok(())
    }

    /// v6.1.3 — `SHOW PUBLICATIONS` row materialisation. Returns
    /// `(name, scope, table_count)` ordered by publication name.
    ///   - `scope` is the human-readable string:
    ///       `"FOR ALL TABLES"` /
    ///       `"FOR TABLE t1, t2"` /
    ///       `"FOR ALL TABLES EXCEPT t1, t2"`.
    ///   - `table_count` is NULL for `AllTables`, the list length
    ///     otherwise. NULLability lets clients distinguish "publish
    ///     everything" from "publish exactly 0 tables" (the v6.1.3
    ///     parser forbids the empty list, but the column shape is
    ///     ready for the v6.1.5 publisher-side semantics).
    fn exec_show_publications(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("scope", DataType::Text, false),
            ColumnSchema::new("table_count", DataType::Int, true),
        ];
        let rows: Vec<Row> = self
            .publications
            .iter()
            .map(|(name, scope)| {
                let (scope_str, count_val) = match scope {
                    spg_sql::ast::PublicationScope::AllTables => {
                        ("FOR ALL TABLES".to_string(), Value::Null)
                    }
                    spg_sql::ast::PublicationScope::ForTables(ts) => (
                        alloc::format!("FOR TABLE {}", ts.join(", ")),
                        Value::Int(i32::try_from(ts.len()).unwrap_or(i32::MAX)),
                    ),
                    spg_sql::ast::PublicationScope::AllTablesExcept(ts) => (
                        alloc::format!("FOR ALL TABLES EXCEPT {}", ts.join(", ")),
                        Value::Int(i32::try_from(ts.len()).unwrap_or(i32::MAX)),
                    ),
                };
                Row::new(alloc::vec![
                    Value::Text(name.clone()),
                    Value::Text(scope_str),
                    count_val,
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v4.1 `SHOW USERS` — `(name, role)` per row, ordered by name.
    fn exec_show_users(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("role", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .users
            .iter()
            .map(|(name, rec)| {
                Row::new(alloc::vec![
                    Value::Text(name.to_string()),
                    Value::Text(rec.role.as_str().to_string()),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    fn exec_create_user(&mut self, s: &CreateUserStatement) -> Result<QueryResult, EngineError> {
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

    fn exec_drop_user(&mut self, name: &str) -> Result<QueryResult, EngineError> {
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
    fn exec_create_function(
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
    fn exec_create_trigger(
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

    fn exec_drop_trigger(
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

    fn exec_drop_function(
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
    fn exec_create_sequence(
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
    fn exec_alter_sequence(
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

    /// v7.17.0 Phase 1.1 — walk a Statement tree and pre-resolve
    /// any sequence FunctionCall nodes inside its Expr slots.
    /// Delegates per-statement-kind: SELECT projection +
    /// WHERE, INSERT VALUES, UPDATE SET, DELETE WHERE.
    fn pre_resolve_sequence_calls_in_statement(
        &mut self,
        stmt: &mut Statement,
    ) -> Result<(), EngineError> {
        match stmt {
            Statement::Select(s) => self.pre_resolve_sequence_calls_in_select(s),
            Statement::Insert(s) => {
                for tuple in &mut s.rows {
                    for cell in tuple.iter_mut() {
                        self.resolve_sequence_calls_in_expr(cell)?;
                    }
                }
                Ok(())
            }
            Statement::Update(s) => {
                for (_col, expr) in &mut s.assignments {
                    self.resolve_sequence_calls_in_expr(expr)?;
                }
                if let Some(w) = &mut s.where_ {
                    self.resolve_sequence_calls_in_expr(w)?;
                }
                Ok(())
            }
            Statement::Delete(s) => {
                if let Some(w) = &mut s.where_ {
                    self.resolve_sequence_calls_in_expr(w)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn pre_resolve_sequence_calls_in_select(
        &mut self,
        s: &mut spg_sql::ast::SelectStatement,
    ) -> Result<(), EngineError> {
        for item in &mut s.items {
            match item {
                spg_sql::ast::SelectItem::Expr { expr, .. } => {
                    self.resolve_sequence_calls_in_expr(expr)?;
                }
                spg_sql::ast::SelectItem::Wildcard => {}
            }
        }
        if let Some(w) = &mut s.where_ {
            self.resolve_sequence_calls_in_expr(w)?;
        }
        Ok(())
    }

    /// v7.17.0 Phase 1.1 — walk an Expr tree and pre-resolve any
    /// `nextval(name)` / `currval(name)` / `setval(name, value[,
    /// is_called])` FunctionCall nodes by calling the catalog and
    /// replacing the node with the resulting `Expr::Literal`.
    /// Used by INSERT VALUES / UPDATE SET / DEFAULT eval so the
    /// row-eval path sees pre-computed sequence values instead of
    /// needing mutable catalog access mid-eval.
    #[allow(clippy::too_many_lines)]
    fn resolve_sequence_calls_in_expr(&mut self, expr: &mut Expr) -> Result<(), EngineError> {
        match expr {
            Expr::Literal(_) | Expr::Column(_) | Expr::Placeholder(_) => Ok(()),
            Expr::FunctionCall { name, args } => {
                // Descend first so nested calls — e.g.
                // setval('seq', currval('other')) — resolve
                // innermost-first.
                for a in args.iter_mut() {
                    self.resolve_sequence_calls_in_expr(a)?;
                }
                let lc = name.to_ascii_lowercase();
                if lc == "nextval" || lc == "currval" || lc == "setval" {
                    let v = self.eval_sequence_call(&lc, args)?;
                    *expr = Expr::Literal(value_to_literal(v));
                } else if lc == "pg_get_serial_sequence" && args.len() == 2 {
                    // v7.29 (round-23a) — resolves to the implicit
                    // sequence name so the pg_dump idiom
                    // `setval(pg_get_serial_sequence('t','c'), n)`
                    // works (the setval arm receives a literal).
                    let lit = |e: &Expr| -> Option<String> {
                        match e {
                            Expr::Literal(spg_sql::ast::Literal::String(v)) => {
                                let t = v.strip_prefix("public.").unwrap_or(v).trim_matches('"');
                                Some(t.to_string())
                            }
                            _ => None,
                        }
                    };
                    if let (Some(t), Some(c)) = (lit(&args[0]), lit(&args[1])) {
                        let is_serial = self.active_catalog().get(&t).is_some_and(|tb| {
                            tb.schema()
                                .columns
                                .iter()
                                .any(|col| col.name == c && col.auto_increment)
                        });
                        *expr = if is_serial {
                            Expr::Literal(spg_sql::ast::Literal::String(alloc::format!(
                                "public.{t}_{c}_seq"
                            )))
                        } else {
                            Expr::Literal(spg_sql::ast::Literal::Null)
                        };
                    }
                }
                Ok(())
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_sequence_calls_in_expr(lhs)?;
                self.resolve_sequence_calls_in_expr(rhs)
            }
            Expr::Unary { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::Cast { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::IsNull { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::Like { expr, pattern, .. } => {
                self.resolve_sequence_calls_in_expr(expr)?;
                self.resolve_sequence_calls_in_expr(pattern)
            }
            Expr::Extract { source, .. } => self.resolve_sequence_calls_in_expr(source),
            Expr::Array(items) => {
                for it in items.iter_mut() {
                    self.resolve_sequence_calls_in_expr(it)?;
                }
                Ok(())
            }
            // Window / subquery / etc — sequence calls inside these
            // are uncommon and require separate row-eval; leave
            // untouched for now and rely on the eval-time error
            // (no sequence_resolver attached).
            _ => Ok(()),
        }
    }

    /// v7.29 (mailrs round-23a) — SERIAL/BIGSERIAL columns get their
    /// PG-style implicit sequence `<table>_<column>_seq` ON FIRST
    /// ADDRESS rather than at CREATE TABLE time, so pre-7.29 data
    /// directories gain addressability without a storage migration.
    /// The sequence is born synced to the column's current MAX so
    /// `nextval` immediately after creation continues the series.
    fn ensure_implicit_sequence(&mut self, seq_name: &str) {
        if self.active_catalog().sequences().contains_key(seq_name) {
            return;
        }
        let Some(rest) = seq_name.strip_suffix("_seq") else {
            return;
        };
        let mut found: Option<(String, String, i64)> = None;
        for tname in self.active_catalog().table_names() {
            let Some(table) = self.active_catalog().get(&tname) else {
                continue;
            };
            for (i, col) in table.schema().columns.iter().enumerate() {
                if col.auto_increment && alloc::format!("{tname}_{}", col.name) == rest {
                    let next = table.next_auto_value(i).unwrap_or(1);
                    found = Some((tname.clone(), col.name.clone(), next - 1));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((tname, cname, last)) = found else {
            return;
        };
        let def = spg_storage::SequenceDef {
            name: seq_name.to_string(),
            data_type: spg_storage::SequenceDataType::BigInt,
            start: 1,
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: Some((tname, cname)),
            last_value: last.max(0),
            is_called: last > 0,
        };
        let _ = self.active_catalog_mut().create_sequence(def, true);
    }

    /// v7.17.0 Phase 1.1 — evaluate a single nextval/currval/
    /// setval call. `args` are already pre-resolved Expr nodes
    /// (literals) — we extract their constant values.
    fn eval_sequence_call(&mut self, op: &str, args: &[Expr]) -> Result<Value, EngineError> {
        if args.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "{op}() takes at least one argument"
            )));
        }
        let seq_name = match &args[0] {
            Expr::Literal(spg_sql::ast::Literal::String(s)) => {
                // v7.17 dump-compat — pg_dump emits sequence
                // names schema-qualified (`'public.posts_id_seq'`).
                // SPG is single-schema; strip a leading
                // `public.` / `pg_catalog.` so the catalog lookup
                // matches the bare-name CREATE SEQUENCE used.
                let trimmed = s
                    .strip_prefix("public.")
                    .or_else(|| s.strip_prefix("pg_catalog."))
                    .unwrap_or(s);
                trimmed.to_string()
            }
            // v7.17 dump-compat — pg_dump also emits
            // `nextval('public.posts_id_seq'::regclass)`
            // where the cast wraps the literal. Peel the cast
            // and continue.
            Expr::Cast { expr, .. } => {
                if let Expr::Literal(spg_sql::ast::Literal::String(s)) = expr.as_ref() {
                    let trimmed = s
                        .strip_prefix("public.")
                        .or_else(|| s.strip_prefix("pg_catalog."))
                        .unwrap_or(s);
                    trimmed.to_string()
                } else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "{op}() first argument must be a literal sequence name"
                    )));
                }
            }
            other => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{op}() first argument must be a literal sequence name, got {other:?}"
                )));
            }
        };
        self.ensure_implicit_sequence(&seq_name);
        match op {
            "nextval" => {
                let v = self
                    .active_catalog_mut()
                    .sequence_next_value(&seq_name)
                    .map_err(EngineError::Storage)?;
                Ok(Value::BigInt(v))
            }
            "currval" => {
                let v = self
                    .active_catalog()
                    .sequence_current_value(&seq_name)
                    .map_err(EngineError::Storage)?;
                Ok(Value::BigInt(v))
            }
            "setval" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "setval() takes 2 or 3 arguments, got {}",
                        args.len()
                    )));
                }
                let value = match &args[1] {
                    Expr::Literal(spg_sql::ast::Literal::Integer(n)) => *n,
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "setval() value argument must be a literal integer, got {other:?}"
                        )));
                    }
                };
                let is_called = if args.len() == 3 {
                    match &args[2] {
                        Expr::Literal(spg_sql::ast::Literal::Bool(b)) => *b,
                        other => {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "setval() is_called argument must be a literal BOOL, got {other:?}"
                            )));
                        }
                    }
                } else {
                    true
                };
                let v = self
                    .active_catalog_mut()
                    .sequence_set_value(&seq_name, value, is_called)
                    .map_err(EngineError::Storage)?;
                Ok(Value::BigInt(v))
            }
            other => Err(EngineError::Unsupported(alloc::format!(
                "unknown sequence op {other:?}"
            ))),
        }
    }

    /// v7.17.0 Phase 1.2 — find every catalog VIEW referenced in
    /// the SELECT's FROM / JOIN graph, re-parse each view's body
    /// source, and prepend it as a synthetic CTE on the
    /// returned SelectStatement. Returns `None` when no view
    /// references are found (caller proceeds with the original
    /// statement); returns `Some(rewritten)` otherwise (caller
    /// re-runs exec_select_cancel on the rewritten form so the
    /// regular CTE materialiser handles it).
    fn expand_views_in_select(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        let cat = self.active_catalog();
        let mut referenced: Vec<String> = Vec::new();
        if let Some(from) = &stmt.from {
            collect_view_refs(&from.primary, cat, &mut referenced);
            for j in &from.joins {
                collect_view_refs(&j.table, cat, &mut referenced);
            }
        }
        // Don't expand a view name that's already shadowed by a
        // CTE on the same SELECT — the CTE wins per PG.
        referenced.retain(|n| !stmt.ctes.iter().any(|c| c.name == *n));
        if referenced.is_empty() {
            return Ok(None);
        }
        let mut new_ctes: Vec<spg_sql::ast::Cte> = Vec::with_capacity(referenced.len());
        for name in &referenced {
            let view = cat.views().get(name).ok_or_else(|| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(alloc::format!(
                    "view {name:?} disappeared mid-expansion"
                )))
            })?;
            let parsed = spg_sql::parser::parse_statement(&view.body).map_err(|e| {
                EngineError::Unsupported(alloc::format!("view {name:?} body re-parse failed: {e}"))
            })?;
            let Statement::Select(body) = parsed else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "view {name:?} body is not a SELECT (catalog corruption)"
                )));
            };
            new_ctes.push(spg_sql::ast::Cte {
                name: name.clone(),
                body,
                recursive: false,
                column_overrides: view.columns.clone(),
            });
        }
        let mut out = stmt.clone();
        // Prepend so view CTEs are visible to caller-supplied CTEs.
        new_ctes.extend(out.ctes);
        out.ctes = new_ctes;
        Ok(Some(out))
    }

    /// v7.17.0 Phase 1.2 — `CREATE VIEW` engine path. Stores the
    /// Display-rendered body verbatim in the catalog; SELECT-from-
    /// view at exec time re-parses + prepends as a synthetic CTE.
    fn exec_create_view(
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
    fn exec_create_type(
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
    fn exec_create_domain(
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
    fn exec_drop_domain(
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
    fn exec_create_schema(
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
    fn exec_drop_schema(
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
    fn exec_drop_type(
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
    fn exec_create_materialized_view(
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
    fn exec_refresh_materialized_view(
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
    fn exec_drop_materialized_view(
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
    fn exec_drop_view(
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
    fn exec_drop_sequence(
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

    /// v4.4 `UPDATE <table> SET col = expr [, ...] [WHERE cond]`.
    /// Filter pass uses the same WHERE eval as `exec_select`. Per
    /// matched row, evaluate each RHS expression against the *old*
    /// row, then call `Table::update_row` which rebuilds indices.
    /// Indexed columns are correctly reflected because rebuild
    /// happens after the cell rewrite.
    fn exec_update_cancel(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.12.5 — snapshot BEFORE/AFTER UPDATE row triggers + the
        // session FTS config before the table mut-borrow opens (the
        // INSERT path uses the same pattern). Empty vecs are the
        // common "no triggers on this table" fast path.
        // v7.13.0 — UPDATE triggers carry an optional `UPDATE OF
        // cols` filter. The filter is paired with each function so
        // the per-row fire loop can skip when no listed column
        // actually differs between OLD and NEW.
        let before_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "BEFORE");
        let after_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: if the WHERE is a PK equality and matches a cold-
        // tier row, promote it back to the hot tier *before* the
        // hot-row walk. The promote pushes the row to the end of
        // `table.rows`, where the upcoming SET-evaluation loop will
        // pick it up and apply the assignments. Lookups for the key
        // never observe a gap because `promote_cold_row` inserts the
        // hot row before retiring the cold locator.
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                // Promote may be a no-op (key is hot-only or absent);
                // we don't care about the return value here — the
                // subsequent hot walk will either match or not.
                let _ = self
                    .active_catalog_mut()
                    .promote_cold_row(&stmt.table, &idx_name, &key);
            }
        }

        // v7.12.1 — cache session FTS config before the table
        // mut-borrow (same reason as exec_delete).
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        // v7.17.0 Phase 2.1 — snapshot the clock pointer before
        // we hold the catalog mutably so ON UPDATE runtime
        // overrides see the engine wall clock.
        let clock_for_on_update = self.clock;
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        // Resolve each SET target to a column position once, validate
        // up front so a typo'd column doesn't leave a partial mutation
        // behind.
        let mut targets: Vec<(usize, &Expr)> = Vec::with_capacity(stmt.assignments.len());
        for (col, expr) in &stmt.assignments {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == *col)
                .ok_or_else(|| {
                    EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                })?;
            targets.push((pos, expr));
        }
        // v7.17.0 Phase 2.1 — for every column with an
        // `ON UPDATE CURRENT_TIMESTAMP` binding that the caller
        // did NOT explicitly set, schedule an automatic override.
        // Reuses `eval_runtime_default_free` so the same
        // canonical runtime-expression whitelist (now /
        // current_timestamp / current_date / …) governs both
        // DEFAULT and ON UPDATE.
        let mut on_update_overrides: Vec<(usize, String)> = Vec::new();
        for (i, col) in schema_cols.iter().enumerate() {
            if targets.iter().any(|(p, _)| *p == i) {
                continue;
            }
            if let Some(src) = &col.on_update_runtime {
                on_update_overrides.push((i, src.clone()));
            }
        }
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()))
            .with_default_text_search_config(ts_cfg.as_deref());
        // Walk candidate rows, evaluate WHERE then SET
        // expressions. We gather (position, new_values) tuples
        // first and apply them afterwards so the WHERE/RHS
        // evaluation reads the original row state — matches PG
        // semantics (UPDATE doesn't see its own writes).
        //
        // v7.20 P4 — index seek: a single-column equality WHERE
        // on an indexed column narrows the walk from
        // O(table.rows()) to O(matches). The full WHERE still
        // re-evaluates per candidate (the seek may be an
        // over-approximation under AND-composites), so semantics
        // are unchanged. profile: the bench's `UPDATE … WHERE
        // id = $1` on a 5 000-row table was a ~1.3 ms full scan
        // per statement; with the seek it's ~2 µs.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let mut planned: Vec<(usize, Vec<Value>)> = Vec::new();
        let candidate_positions: Vec<usize> = match &seek_positions {
            Some(list) => list.clone(),
            None => (0..table.row_count()).collect(),
        };
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            // v4.5: cooperative cancel checkpoint every 256 rows so
            // a runaway UPDATE without WHERE doesn't drag past the
            // server's query-timeout watchdog.
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut new_vals = row.values.clone();
            for (pos, expr) in &targets {
                let v = eval::eval_expr(expr, row, &ctx)?;
                let coerced = coerce_value(v, schema_cols[*pos].ty, &schema_cols[*pos].name, *pos)?;
                check_unsigned_range(&coerced, &schema_cols[*pos], *pos)?;
                new_vals[*pos] = coerced;
            }
            // v7.17.0 Phase 2.1 — apply ON UPDATE overrides for
            // any column the SET clause didn't touch.
            for (pos, src) in &on_update_overrides {
                let v = eval_runtime_default_free(src, schema_cols[*pos].ty, clock_for_on_update)?;
                new_vals[*pos] = v;
            }
            planned.push((i, new_vals));
        }
        // planned must stay position-sorted: downstream passes
        // (FK pairing, trigger walks, the apply loop) iterate it
        // assuming ascending row order, which the full-scan path
        // guaranteed implicitly.
        planned.sort_by_key(|(i, _)| *i);
        // v7.6.6 — capture pre-update row values for the FK
        // enforcement passes below. `planned` carries new values
        // only; pair them with the old row.
        let plan_with_old: Vec<(usize, Vec<Value>, Vec<Value>)> = planned
            .iter()
            .map(|(pos, new_vals)| (*pos, table.rows()[*pos].values.clone(), new_vals.clone()))
            .collect();
        let self_fks = table.schema().foreign_keys.clone();
        // v7.12.5 — `affected` is computed post-BEFORE-trigger
        // below (triggers may RETURN NULL to skip individual
        // rows). The pre-trigger len shape is no longer accurate.
        // Release mutable borrow on `table` for the FK passes.
        let _ = table;
        // v7.6.6 — Stage 2a: outbound FK check. For every row whose
        // local FK columns changed, the new value must exist in the
        // parent.
        if !self_fks.is_empty() {
            let new_rows: Vec<Vec<Value>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            enforce_fk_inserts(self.active_catalog(), &stmt.table, &self_fks, &new_rows)?;
        }
        // v7.13.0 — CHECK constraint enforcement on UPDATE
        // (mailrs round-5 G3). Predicates evaluated against the
        // candidate post-UPDATE row; false rejects the UPDATE.
        {
            let new_rows: Vec<Vec<Value>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            enforce_check_constraints(self.active_catalog(), &stmt.table, &new_rows)?;
        }
        // v7.6.6 — Stage 2b: inbound FK check. For every row that
        // changed value in a column that *some other table* uses as
        // a FK parent column, react per `on_update` action.
        let child_plan =
            plan_fk_parent_updates(self.active_catalog(), &stmt.table, &plan_with_old)?;
        // Stage 3a — apply each child-side action.
        for step in &child_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // Stage 3b — apply the original UPDATE.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v7.12.5 — fire BEFORE/AFTER UPDATE row-level triggers
        // around the apply loop. BEFORE sees NEW=candidate +
        // OLD=current; may rewrite NEW or RETURN NULL to skip.
        // AFTER sees NEW=post-write + OLD=pre-write (both read-
        // only).
        //
        // Filter `planned` through the BEFORE pass first so the
        // RETURNING snapshot reflects what actually got written
        // (triggers may rewrite cells, including a cancellation).
        let mut applied_after_before: Vec<(usize, Row, Row)> = Vec::with_capacity(planned.len());
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        for (pos, new_vals) in &planned {
            let old_row = table.rows()[*pos].clone();
            let mut new_row = Row::new(new_vals.clone());
            let mut skip = false;
            for (fd, filter) in &before_update_triggers {
                // v7.13.0 — `UPDATE OF cols` filter (mailrs round-5
                // G7). Skip this trigger when the filter is set and
                // no listed column actually differs between OLD and
                // NEW for this row.
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    false,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => new_row = r,
                    triggers::TriggerOutcome::Skip => {
                        skip = true;
                        break;
                    }
                }
            }
            if !skip {
                applied_after_before.push((*pos, new_row, old_row));
            }
        }
        // v7.9.4 — snapshot post-update values for RETURNING (post-
        // BEFORE-trigger because triggers can rewrite cells).
        let updated_for_returning: Vec<Vec<Value>> = if stmt.returning.is_some() {
            applied_after_before
                .iter()
                .map(|(_pos, new_row, _old)| new_row.values.clone())
                .collect()
        } else {
            Vec::new()
        };
        let affected = applied_after_before.len();
        // Apply, then fire AFTER triggers per row. AFTER runs read-
        // only against the freshly-written row; v7.12.4-shape
        // assignment errors with a clear message.
        for (pos, new_row, old_row) in applied_after_before {
            table.update_row(pos, new_row.values.clone())?;
            for (fd, filter) in &after_update_triggers {
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                let (_outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    true,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
            }
        }
        let _ = table;
        // v7.12.7 — drain trigger-emitted embedded SQL for this UPDATE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for UPDATE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.9.4 — RETURNING projection.
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, updated_for_returning);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Collects matching
    /// positions then delegates to `Table::delete_rows` (single index
    /// rebuild for the batch).
    /// v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ `MERGE` execution.
    ///
    /// Semantics:
    ///   * Resolve `target` and `source` tables (catalog reads).
    ///   * Build a combined `(target_alias.col, source_alias.col)`
    ///     schema so the ON / WHEN AND / SET / VALUES expressions
    ///     resolve through the standard qualifier-aware resolver.
    ///   * Pass 1: walk every source row × every target hot row,
    ///     evaluate ON, then pick the first WHEN clause that fits
    ///     (`Matched` if any target row matched, `NotMatched`
    ///     otherwise; AND-condition must hold). Collect the action
    ///     plan as `(deletes, updates, inserts)` so the apply pass
    ///     reads the original target row state.
    ///   * Pass 2: apply the plan against the target's mutable row
    ///     vector. Deletes execute by index in descending order so
    ///     earlier indices remain stable; updates next; inserts
    ///     last (matching PG's "INSERT branch sees the post-delete
    ///     state" behaviour for the common upsert shape).
    ///
    /// v7.17 simplifications (documented limitations):
    ///   * No triggers / WAL plumbing (MVP); MERGE rows don't fire
    ///     INSERT / UPDATE / DELETE row triggers in v7.17.
    ///   * No cardinality check (PG-canonical: "MERGE command
    ///     cannot affect row a second time" — SPG silently applies
    ///     the last action for a target row covered twice).
    ///   * Source must be a catalog-resolvable table (no subquery
    ///     source); RETURNING / BY SOURCE / BY TARGET unsupported.
    fn exec_merge_cancel(
        &mut self,
        stmt: &spg_sql::ast::MergeStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let target_alias = stmt
            .target_alias
            .clone()
            .unwrap_or_else(|| stmt.target.clone());
        let source_alias = stmt
            .source_alias
            .clone()
            .unwrap_or_else(|| stmt.source.clone());
        let (target_cols, target_rows_snapshot) = {
            let t = self.active_catalog().get(&stmt.target).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
            (
                t.schema().columns.clone(),
                t.rows().iter().cloned().collect::<Vec<Row>>(),
            )
        };
        let (source_cols, source_rows) = {
            let s = self.active_catalog().get(&stmt.source).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.source.clone(),
                })
            })?;
            (
                s.schema().columns.clone(),
                s.rows().iter().cloned().collect::<Vec<Row>>(),
            )
        };
        // Composite schema: target_alias.col ... source_alias.col ...
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let combined_ctx = EvalContext::new(&combined_schema, None);
        // Source-only context for WHEN NOT MATCHED actions (no
        // matched target row exists — the source-side qualified
        // columns must still resolve).
        let mut source_only_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let source_only_ctx = EvalContext::new(&source_only_schema, None);
        let target_arity = target_cols.len();
        let source_arity = source_cols.len();

        // Resolve INSERT column positions once (validate names).
        // For each clause that's an INSERT, map column names → target positions.
        let mut delete_indices: Vec<usize> = Vec::new();
        let mut updates: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut inserts: Vec<Vec<Value>> = Vec::new();
        let mut affected: usize = 0;

        for (src_idx, src_row) in source_rows.iter().enumerate() {
            if src_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            // Find every matched target index (per the ON predicate).
            let mut matched_targets: Vec<usize> = Vec::new();
            for (t_idx, t_row) in target_rows_snapshot.iter().enumerate() {
                let mut combined_vals = t_row.values.clone();
                combined_vals.extend(src_row.values.iter().cloned());
                let combined_row = Row::new(combined_vals);
                let cond = eval::eval_expr(&stmt.on, &combined_row, &combined_ctx)?;
                if matches!(cond, Value::Bool(true)) {
                    matched_targets.push(t_idx);
                }
            }
            let is_matched = !matched_targets.is_empty();
            // Pick the first WHEN clause whose kind agrees with
            // `is_matched` and whose AND condition (if any) holds.
            // AND condition for MATCHED: evaluated against the
            // first matched target row × source. For NOT MATCHED:
            // evaluated with target side NULL-padded.
            let fired_clause = stmt.clauses.iter().find(|c| {
                let kind_ok = match c.matched {
                    spg_sql::ast::MergeMatched::Matched => is_matched,
                    spg_sql::ast::MergeMatched::NotMatched => !is_matched,
                };
                if !kind_ok {
                    return false;
                }
                let Some(cond_expr) = &c.condition else {
                    return true;
                };
                let row = if is_matched {
                    let t = &target_rows_snapshot[matched_targets[0]];
                    let mut vals = t.values.clone();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                } else {
                    let mut vals: Vec<Value> = (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                };
                let ctx_ref = if is_matched {
                    &combined_ctx
                } else {
                    &source_only_ctx
                };
                matches!(
                    eval::eval_expr(cond_expr, &row, ctx_ref),
                    Ok(Value::Bool(true))
                )
            });
            let Some(clause) = fired_clause else { continue };
            match &clause.action {
                spg_sql::ast::MergeAction::DoNothing => {}
                spg_sql::ast::MergeAction::Delete => {
                    for &t_idx in &matched_targets {
                        if !delete_indices.contains(&t_idx) {
                            delete_indices.push(t_idx);
                            affected += 1;
                        }
                    }
                }
                spg_sql::ast::MergeAction::Update { assignments } => {
                    // Pre-resolve SET targets to target column positions.
                    let mut planned_sets: Vec<(usize, &Expr)> =
                        Vec::with_capacity(assignments.len());
                    for (col, expr) in assignments {
                        let pos =
                            target_cols
                                .iter()
                                .position(|c| c.name == *col)
                                .ok_or_else(|| {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                })?;
                        planned_sets.push((pos, expr));
                    }
                    for &t_idx in &matched_targets {
                        let t_row = &target_rows_snapshot[t_idx];
                        let mut new_values = t_row.values.clone();
                        let mut combined_vals = t_row.values.clone();
                        combined_vals.extend(src_row.values.iter().cloned());
                        let combined_row = Row::new(combined_vals);
                        for (pos, expr) in &planned_sets {
                            let raw = eval::eval_expr(expr, &combined_row, &combined_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[*pos].ty,
                                &target_cols[*pos].name,
                                *pos,
                            )?;
                            new_values[*pos] = coerced;
                        }
                        updates.push((t_idx, new_values));
                        affected += 1;
                    }
                }
                spg_sql::ast::MergeAction::Insert { columns, values } => {
                    // For INSERT NOT MATCHED, target side is NULL-padded.
                    let mut vals: Vec<Value> = (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    let synth_row = Row::new(vals);
                    let mut new_row_values: Vec<Value> =
                        (0..target_arity).map(|_| Value::Null).collect();
                    for (col, expr) in columns.iter().zip(values.iter()) {
                        let pos =
                            target_cols
                                .iter()
                                .position(|c| c.name == *col)
                                .ok_or_else(|| {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                })?;
                        let raw = eval::eval_expr(expr, &synth_row, &source_only_ctx)?;
                        let coerced =
                            coerce_value(raw, target_cols[pos].ty, &target_cols[pos].name, pos)?;
                        new_row_values[pos] = coerced;
                    }
                    inserts.push(new_row_values);
                    affected += 1;
                }
            }
        }
        let _ = source_arity; // captured for symmetry; cancellation cost negligible.

        // Apply the plan to the target table.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.target)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
        // Apply updates first (in-place), then deletes (one batch),
        // then inserts. The storage API uses `update_row(pos,
        // new_values)`, `delete_rows(&[positions])`, and `insert(row)`.
        for (idx, new_vals) in &updates {
            table
                .update_row(*idx, new_vals.clone())
                .map_err(EngineError::Storage)?;
        }
        if !delete_indices.is_empty() {
            table.delete_rows(&delete_indices);
        }
        for vals in inserts {
            table.insert(Row::new(vals)).map_err(EngineError::Storage)?;
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: affected > 0,
        })
    }

    fn exec_delete_cancel(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.12.5 — snapshot BEFORE/AFTER DELETE row triggers + the
        // session FTS config before the mut borrow (same shape as
        // INSERT / UPDATE).
        let before_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "BEFORE");
        let after_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: PK-targeted DELETE → first retire any cold-tier
        // locator for the key. The cold row body stays in the
        // segment (becoming shadowed garbage that a future
        // compaction pass reclaims) but the index no longer
        // resolves it. The shadow count contributes to the
        // affected total; the subsequent hot walk handles any hot
        // rows for the same key.
        let mut cold_shadow_count: usize = 0;
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                cold_shadow_count = self
                    .active_catalog_mut()
                    .shadow_cold_row(&stmt.table, &idx_name, &key)
                    .unwrap_or(0);
            }
        }

        // v7.12.1 — cache the session FTS config as an owned
        // String before the mutable table borrow below; the
        // ctx-builder then references it via `as_deref` so the
        // immutable read of `session_params` doesn't conflict
        // with the mut borrow chain.
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()))
            .with_default_text_search_config(ts_cfg.as_deref());
        let mut positions: Vec<usize> = Vec::new();
        // v7.6.3 — collect every to-delete row's full Value tuple
        // alongside its position, so the FK enforcement pass can
        // run after the mut borrow drops.
        let mut to_delete_rows: Vec<Vec<Value>> = Vec::new();
        // v7.20 P4 — index seek (same shape as exec_update_cancel):
        // an equality WHERE on an indexed column narrows the walk
        // to the matching hot positions; the full WHERE still
        // re-evaluates per candidate. Downstream passes assume
        // ascending position order, so the seek result is sorted.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let candidate_positions: Vec<usize> = match seek_positions {
            Some(mut list) => {
                list.sort_unstable();
                list
            }
            None => (0..table.row_count()).collect(),
        };
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            let keep = if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                !matches!(cond, Value::Bool(true))
            } else {
                false
            };
            if !keep {
                positions.push(i);
                to_delete_rows.push(row.values.clone());
            }
        }
        // v7.6.3 / v7.6.4 — Stage 2: FK enforcement on the immutable
        // catalog. Release the mut borrow and run reverse-scan
        // against every child table whose FK targets this table.
        // RESTRICT / NoAction raise an error; CASCADE returns a
        // cascade plan that stage 3 applies after the primary delete.
        // SET NULL / SET DEFAULT remain Unsupported until v7.6.5.
        let _ = table;
        // v7.12.5 — BEFORE DELETE row-level triggers. Each fires
        // with NEW=None / OLD=pre-delete row; RETURN OLD (or NEW)
        // = proceed, RETURN NULL = skip the row entirely. The
        // filter must run BEFORE the FK cascade plan so cascaded
        // child rows track the trigger's skip-decision on the
        // parent.
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        if !before_delete_triggers.is_empty() {
            let mut filtered_positions: Vec<usize> = Vec::with_capacity(positions.len());
            let mut filtered_old_rows: Vec<Vec<Value>> = Vec::with_capacity(to_delete_rows.len());
            for (pos, old_vals) in positions.iter().zip(to_delete_rows.iter()) {
                let old_row = Row::new(old_vals.clone());
                let mut cancel_this = false;
                for fd in &before_delete_triggers {
                    let (outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        false,
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                    if matches!(outcome, triggers::TriggerOutcome::Skip) {
                        cancel_this = true;
                        break;
                    }
                }
                if !cancel_this {
                    filtered_positions.push(*pos);
                    filtered_old_rows.push(old_vals.clone());
                }
            }
            positions = filtered_positions;
            to_delete_rows = filtered_old_rows;
        }
        let cascade_plan = plan_fk_parent_deletions(
            self.active_catalog(),
            &stmt.table,
            &positions,
            &to_delete_rows,
        )?;
        // Stage 3a — apply each FK child step (SET NULL / SET
        // DEFAULT / CASCADE delete) before deleting the parent.
        // The plan is already ordered: nulls/defaults first, then
        // cascade deletes (so a row mutated and later deleted
        // surfaces as deleted — though v7.6.5 doesn't produce
        // that overlap today).
        for step in &cascade_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // Stage 3b — actually delete the original target rows.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let affected = table.delete_rows(&positions) + cold_shadow_count;
        let _ = table;
        // v7.12.5 — AFTER DELETE row-level triggers fire post-write
        // with NEW=None / OLD=pre-delete row (each from the
        // already-snapshotted to_delete_rows). Return value is
        // ignored (matches PG AFTER semantics).
        if !after_delete_triggers.is_empty() {
            for old_vals in &to_delete_rows {
                let old_row = Row::new(old_vals.clone());
                for fd in &after_delete_triggers {
                    let (_outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        true,
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                }
            }
        }
        // v7.12.7 — drain trigger-emitted embedded SQL for this DELETE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for DELETE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.9.4 — RETURNING projection over the soon-to-be-gone
        // rows. `to_delete_rows` was snapshotted in stage 1 before
        // mutation, so the projection sees the pre-delete state
        // (matches PG semantics: DELETE RETURNING returns the row
        // as it was just before removal).
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, to_delete_rows);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// `SHOW TABLES` — one row per table in the active catalog.
    /// Column name is `name` so result-set consumers can downstream
    /// `SELECT name FROM ...` style logic if needed.
    /// v4.26: `EXPLAIN [ANALYZE] <select>`. Returns a single-column
    /// `QUERY PLAN` text table — first line names the top operator
    /// (Scan / Aggregate / Window / etc.), indented children list
    /// FROM joins, WHERE filters, ORDER BY / LIMIT, projection
    /// shape, and any active index hits. `ANALYZE` execs the inner
    /// SELECT and appends actual-row + elapsed-micros annotations.
    #[allow(clippy::format_push_string)]
    fn exec_explain(
        &self,
        e: &spg_sql::ast::ExplainStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let mut lines = Vec::<String>::new();
        explain_select(&e.inner, self, 0, &mut lines);
        if e.suggest {
            // v6.8.3 — index advisor. Walks the SELECT's FROM
            // tables + WHERE column refs; for each (table, column)
            // pair that lacks an index, append a SUGGEST line with
            // a copy-pastable `CREATE INDEX` statement. This is a
            // pure-syntax heuristic — no cardinality estimation —
            // matching the v6.8.3 design intent of "tell the
            // operator where indexes are missing", not "give the
            // mathematically optimal index set".
            let suggestions = build_index_suggestions(&e.inner, self);
            for s in suggestions {
                lines.push(s);
            }
        } else if e.analyze {
            // v6.2.4 — EXPLAIN ANALYZE annotates each operator line
            // with `(rows=N)` where the row count is computable
            // without re-executing the full query:
            //   - Top-level operator (first non-indented line):
            //     rows = final result.len()
            //   - "From: <table> [full scan]" lines: rows =
            //     table.rows().len() (catalog read; no execution)
            //   - "From: <table> [index seek]": indeterminate —
            //     the index step would need re-execution; v6.2.5
            //     adds per-operator wall-clock + hot/cold rows
            //     instrumentation that makes this concrete.
            //   - Everything else: marked `(—)` so the surface
            //     stays well-defined without silently dropping
            //     stats. v6.2.5 fills in via inline executor
            //     instrumentation.
            // Total elapsed lands on a trailing `Total: …` line.
            let started = self.clock.map(|f| f());
            let exec = self.exec_select_cancel(&e.inner, cancel)?;
            let elapsed_micros = match (self.clock, started) {
                (Some(f), Some(s)) => Some(f().saturating_sub(s)),
                _ => None,
            };
            let row_count = if let QueryResult::Rows { rows, .. } = &exec {
                rows.len()
            } else {
                0
            };
            annotate_explain_lines(&mut lines, row_count, self);
            let mut total = alloc::format!("Total: rows={row_count}");
            if let Some(us) = elapsed_micros {
                total.push_str(&alloc::format!(" elapsed={us}us"));
            }
            lines.push(total);
        }
        let columns = alloc::vec![ColumnSchema::new("QUERY PLAN", DataType::Text, false)];
        let rows: Vec<Row> = lines
            .into_iter()
            .map(|l| Row::new(alloc::vec![Value::Text(l)]))
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

    fn exec_show_tables(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("name", DataType::Text, false)];
        let rows: Vec<Row> = self
            .active_catalog()
            .table_names()
            .into_iter()
            .map(|n| Row::new(alloc::vec![Value::Text(n)]))
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-59 — `SHOW CREATE TABLE <t>`. Synthesise
    /// a minimal MySQL-flavoured CREATE TABLE DDL from the
    /// catalog's TableSchema so mysqldump round-trips load against
    /// SPG without splitting init scripts.
    fn exec_show_create_table(&self, name: &str) -> Result<QueryResult, EngineError> {
        let t = self.active_catalog().get(name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: name.into() })
        })?;
        let cols: Vec<String> = t
            .schema()
            .columns
            .iter()
            .map(|c| {
                let ty = render_data_type(c.ty);
                let nullable = if c.nullable { "" } else { " NOT NULL" };
                alloc::format!("  `{}` {}{}", c.name, ty, nullable)
            })
            .collect();
        let mut body = cols.join(",\n");
        // Append UNIQUE / PRIMARY KEY clauses.
        for uc in &t.schema().uniqueness_constraints {
            let col_names: Vec<String> = uc
                .columns
                .iter()
                .map(|&p| {
                    t.schema().columns.get(p).map_or_else(
                        || alloc::format!("col{p}"),
                        |c| alloc::format!("`{}`", c.name),
                    )
                })
                .collect();
            let kw = if uc.is_primary_key {
                "PRIMARY KEY"
            } else {
                "UNIQUE KEY"
            };
            body.push_str(",\n  ");
            body.push_str(&alloc::format!("{kw} ({})", col_names.join(", ")));
        }
        // Foreign keys.
        for fk in &t.schema().foreign_keys {
            let local: Vec<String> = fk
                .local_columns
                .iter()
                .map(|&p| {
                    t.schema().columns.get(p).map_or_else(
                        || alloc::format!("col{p}"),
                        |c| alloc::format!("`{}`", c.name),
                    )
                })
                .collect();
            let parent_cols: Vec<String> =
                if let Some(parent) = self.active_catalog().get(&fk.parent_table) {
                    fk.parent_columns
                        .iter()
                        .map(|&p| {
                            parent.schema().columns.get(p).map_or_else(
                                || alloc::format!("col{p}"),
                                |c| alloc::format!("`{}`", c.name),
                            )
                        })
                        .collect()
                } else {
                    fk.parent_columns
                        .iter()
                        .map(|p| alloc::format!("col{p}"))
                        .collect()
                };
            body.push_str(",\n  ");
            body.push_str(&alloc::format!(
                "FOREIGN KEY ({}) REFERENCES `{}` ({})",
                local.join(", "),
                fk.parent_table,
                parent_cols.join(", ")
            ));
        }
        let ddl = alloc::format!(
            "CREATE TABLE `{}` (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            name,
            body
        );
        let columns = alloc::vec![
            ColumnSchema::new("Table", DataType::Text, false),
            ColumnSchema::new("Create Table", DataType::Text, false),
        ];
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::Text(name.into()),
            Value::Text(ddl),
        ])];
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v7.17.0 Phase 3.P0-60 — `SHOW INDEXES FROM <t>`. MySQL
    /// surface returns one row per (index × column) with 14
    /// columns; v7.17 ships the columns admin probes actually
    /// filter on: Table, Non_unique, Key_name, Seq_in_index,
    /// Column_name, Null, Index_type.
    fn exec_show_indexes(&self, name: &str) -> Result<QueryResult, EngineError> {
        let t = self.active_catalog().get(name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: name.into() })
        })?;
        let columns = alloc::vec![
            ColumnSchema::new("Table", DataType::Text, false),
            ColumnSchema::new("Non_unique", DataType::Int, false),
            ColumnSchema::new("Key_name", DataType::Text, false),
            ColumnSchema::new("Seq_in_index", DataType::Int, false),
            ColumnSchema::new("Column_name", DataType::Text, false),
            ColumnSchema::new("Null", DataType::Text, false),
            ColumnSchema::new("Index_type", DataType::Text, false),
        ];
        let mut rows: Vec<Row> = Vec::new();
        for idx in t.indices() {
            let col = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            let nullable = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or(true, |c| c.nullable);
            rows.push(Row::new(alloc::vec![
                Value::Text(name.into()),
                Value::Int(i32::from(!idx.is_unique)),
                Value::Text(idx.name.clone()),
                Value::Int(1),
                Value::Text(col),
                Value::Text(if nullable {
                    "YES".into()
                } else {
                    String::new()
                }),
                Value::Text("BTREE".into()),
            ]));
        }
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v7.17.0 Phase 3.P0-61 — `SHOW STATUS`. Returns canonical
    /// MySQL server-status counters (2-column `(Variable_name,
    /// Value)`).
    fn exec_show_status(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Variable_name", DataType::Text, false),
            ColumnSchema::new("Value", DataType::Text, false),
        ];
        let pairs: &[(&str, &str)] = &[
            ("Uptime", "0"),
            ("Threads_connected", "1"),
            ("Threads_running", "1"),
            ("Questions", "0"),
            ("Slow_queries", "0"),
            ("Opened_tables", "0"),
            ("Innodb_buffer_pool_pages_total", "0"),
        ];
        let rows: Vec<Row> = pairs
            .iter()
            .map(|(k, v)| {
                Row::new(alloc::vec![
                    Value::Text((*k).into()),
                    Value::Text((*v).into())
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-61 — `SHOW VARIABLES`. Returns server-side
    /// variables MySQL/MariaDB clients probe at connect time.
    fn exec_show_variables(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Variable_name", DataType::Text, false),
            ColumnSchema::new("Value", DataType::Text, false),
        ];
        let mut rows: Vec<Row> = Vec::new();
        let canonical: &[(&str, &str)] = &[
            ("version", "8.0.35-spg"),
            ("version_comment", "SPG dual-stack engine"),
            ("character_set_server", "utf8mb4"),
            ("collation_server", "utf8mb4_0900_ai_ci"),
            ("max_allowed_packet", "67108864"),
            ("autocommit", "ON"),
            ("sql_mode", "STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"),
            ("time_zone", "SYSTEM"),
            ("transaction_isolation", "REPEATABLE-READ"),
        ];
        for &(k, v) in canonical {
            rows.push(Row::new(alloc::vec![
                Value::Text(k.into()),
                Value::Text(v.into()),
            ]));
        }
        // Session-set parameters surface here too.
        for (k, v) in &self.session_params {
            if !canonical.iter().any(|(n, _)| (*n).eq_ignore_ascii_case(k)) {
                rows.push(Row::new(alloc::vec![
                    Value::Text(k.clone()),
                    Value::Text(v.clone()),
                ]));
            }
        }
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-62 — `SHOW PROCESSLIST`. SPG is
    /// single-process so the surface returns one synthetic row
    /// describing the current connection (Id, User, Host, db,
    /// Command, Time, State, Info).
    fn exec_show_processlist(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Id", DataType::Int, false),
            ColumnSchema::new("User", DataType::Text, false),
            ColumnSchema::new("Host", DataType::Text, false),
            ColumnSchema::new("db", DataType::Text, true),
            ColumnSchema::new("Command", DataType::Text, false),
            ColumnSchema::new("Time", DataType::Int, false),
            ColumnSchema::new("State", DataType::Text, true),
            ColumnSchema::new("Info", DataType::Text, true),
        ];
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::Int(1),
            Value::Text("postgres".into()),
            Value::Text("localhost".into()),
            Value::Text("postgres".into()),
            Value::Text("Query".into()),
            Value::Int(0),
            Value::Text("executing".into()),
            Value::Text("SHOW PROCESSLIST".into()),
        ])];
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-58 — `SHOW DATABASES` / `SHOW SCHEMAS`.
    /// SPG is single-database so the result is the canonical MySQL
    /// set every mysql/MariaDB client expects at connect time:
    /// `information_schema`, `mysql`, `performance_schema`, `sys`,
    /// plus a `postgres` slot so dual-stack callers find their
    /// PG-compatible database too.
    fn exec_show_databases(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("Database", DataType::Text, false)];
        let names = [
            "information_schema",
            "mysql",
            "performance_schema",
            "sys",
            "postgres",
        ];
        let rows: Vec<Row> = names
            .iter()
            .map(|n| Row::new(alloc::vec![Value::Text((*n).into())]))
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// `SHOW COLUMNS FROM <table>` — one row per column with the
    /// declared name, SQL type rendering, and nullability flag.
    fn exec_show_columns(&self, table_name: &str) -> Result<QueryResult, EngineError> {
        let table =
            self.active_catalog()
                .get(table_name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: table_name.into(),
                })?;
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("type", DataType::Text, false),
            ColumnSchema::new("nullable", DataType::Bool, false),
        ];
        let rows: Vec<Row> = table
            .schema()
            .columns
            .iter()
            .map(|c| {
                Row::new(alloc::vec![
                    Value::Text(c.name.clone()),
                    Value::Text(alloc::format!("{}", c.ty)),
                    Value::Bool(c.nullable),
                ])
            })
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

    fn exec_begin(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        if self.tx_catalogs.contains_key(&tx_id) {
            return Err(EngineError::TransactionAlreadyOpen);
        }
        self.tx_catalogs.insert(
            tx_id,
            TxState {
                catalog: self.catalog.clone(),
                savepoints: Vec::new(),
            },
        );
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_commit(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .remove(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        self.catalog = state.catalog;
        // All savepoints become permanent at COMMIT and the stack
        // resets for the next TX (`state.savepoints` is discarded with
        // `state`).
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    fn exec_rollback(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        if self.tx_catalogs.remove(&tx_id).is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        // savepoints discarded with the TxState
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_savepoint(&mut self, name: String) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        // PG re-uses an existing savepoint name by dropping the older
        // entry and pushing a fresh one — match that behaviour so
        // application code can `SAVEPOINT sp; ...; SAVEPOINT sp` freely.
        state.savepoints.retain(|(n, _)| n != &name);
        let snapshot = state.catalog.clone();
        state.savepoints.push((name, snapshot));
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_rollback_to_savepoint(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        let pos = state
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // The savepoint stays on the stack (PG semantics): a later
        // `RELEASE` or further `ROLLBACK TO` is still allowed. Everything
        // after it is discarded.
        let snapshot = state.savepoints[pos].1.clone();
        state.savepoints.truncate(pos + 1);
        state.catalog = snapshot;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_release_savepoint(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        let pos = state
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // RELEASE keeps the work since the savepoint, just discards the
        // bookmark plus everything nested under it.
        state.savepoints.truncate(pos);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    /// v6.0.4 — synchronous `ALTER INDEX <name> REBUILD [WITH
    /// (encoding = …)]`. Walks every table in the active catalog
    /// looking for an index matching `stmt.name`, then delegates the
    /// rebuild (including any encoding switch) to
    /// `Table::rebuild_nsw_index`. The "live" non-blocking
    /// optimisation is v6.0.4.1 / v6.1.x territory.
    /// v6.7.2 — `ALTER TABLE t SET hot_tier_bytes = X`. Dispatch
    /// arm. Currently the only setting is `hot_tier_bytes`; later
    /// v6.7.x can extend `AlterTableTarget` without touching this
    /// arm structure.
    fn exec_alter_table(
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

    fn exec_alter_table_subaction(
        &mut self,
        table_name_outer: &str,
        target: spg_sql::ast::AlterTableTarget,
    ) -> Result<(), EngineError> {
        // Inner helper retains the s.name closure shape; alias to `s`
        // for minimal diff against the v7.13.0 body.
        struct S<'a> {
            name: &'a str,
        }
        let s = S {
            name: table_name_outer,
        };
        match target {
            spg_sql::ast::AlterTableTarget::SetHotTierBytes(n) => {
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
                })?;
                table.schema_mut().hot_tier_bytes = Some(n);
            }
            spg_sql::ast::AlterTableTarget::AddForeignKey(fk) => {
                // v7.6.8 — resolve FK against the live catalog first
                // (validates parent table, columns, indices). Then
                // verify every existing row in the child table
                // satisfies the new constraint. Then install it.
                let cols_snapshot = self
                    .active_catalog()
                    .get(s.name)
                    .ok_or_else(|| {
                        EngineError::Storage(StorageError::TableNotFound {
                            name: s.name.into(),
                        })
                    })?
                    .schema()
                    .columns
                    .clone();
                let storage_fk =
                    resolve_foreign_key(s.name, &cols_snapshot, fk, self.active_catalog())?;
                // Verify existing rows. Treat them as a virtual
                // INSERT batch — reusing the v7.6.2 enforce helper.
                let existing_rows: Vec<Vec<Value>> = self
                    .active_catalog()
                    .get(s.name)
                    .expect("checked above")
                    .rows()
                    .iter()
                    .map(|r| r.values.clone())
                    .collect();
                enforce_fk_inserts(
                    self.active_catalog(),
                    s.name,
                    core::slice::from_ref(&storage_fk),
                    &existing_rows,
                )?;
                // Reject duplicate constraint name.
                let table = self
                    .active_catalog_mut()
                    .get_mut(s.name)
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
            }
            spg_sql::ast::AlterTableTarget::DropForeignKey { name, if_exists } => {
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
                })?;
                let fks = &mut table.schema_mut().foreign_keys;
                let before = fks.len();
                fks.retain(|f| f.name.as_ref() != Some(&name));
                if fks.len() == before && !if_exists {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "ALTER TABLE DROP CONSTRAINT: no FK named {name:?} on {:?}",
                        s.name
                    )));
                }
                // v7.13.2 mailrs round-6 S7: IF EXISTS silences the miss.
            }
            spg_sql::ast::AlterTableTarget::AddColumn {
                column,
                if_not_exists,
            } => {
                // v7.13.0 — mailrs round-5 G1. Append-only column add
                // with back-fill of the DEFAULT (or NULL) into every
                // existing row. Column positions don't shift, so we
                // skip index rebuild.
                let clock = self.clock;
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
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
                        s.name
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
            }
            spg_sql::ast::AlterTableTarget::AlterColumnType {
                column,
                new_type,
                using,
            } => {
                // v7.13.0 — mailrs round-5 G8. Re-evaluate each
                // row's column value (either through the USING
                // expression if supplied, or as a direct CAST of
                // the existing value) and re-coerce to the new
                // type. Indices on the column get rebuilt.
                let new_data_type = column_type_to_data_type(new_type);
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
                })?;
                let col_pos = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "ALTER COLUMN TYPE: column {column:?} not found on {:?}",
                            s.name
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
            }
            spg_sql::ast::AlterTableTarget::AddTableConstraint(tc) => {
                // v7.14.0 — pg_dump emits PKs as a separate
                // ALTER TABLE ADD CONSTRAINT post-CREATE-TABLE.
                // For PRIMARY KEY / UNIQUE, install a UC entry
                // and the implicit BTree index on the leading
                // column. CHECK: append predicate to schema.
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
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
                                            s.name
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
                                let idx_name = alloc::format!("{}_{leading}_{suffix}", s.name);
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
                                .unwrap_or_else(|| alloc::format!("{}_{leading}_idx", s.name));
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
                                    alloc::format!("{}_{col}_ftidx", s.name)
                                }
                            };
                            let _ = table.add_gin_fulltext_index(idx_name, col);
                        }
                    }
                }
            }
            spg_sql::ast::AlterTableTarget::DropColumn {
                column,
                if_exists,
                cascade,
            } => {
                // v7.13.3 — mailrs round-7 S8. Remove the column +
                // every row's value at that position; drop any index
                // on the column. RESTRICT (default) rejects when an
                // FK on this table or partial-index predicate
                // references the column; CASCADE removes those
                // dependents first.
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
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
                            s.name
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
            }
            spg_sql::ast::AlterTableTarget::SetTriggerEnabled { which, enabled } => {
                // v7.16.1 — mailrs round-9 A.2.b. pg_dump
                // --disable-triggers wraps each table's data
                // block with `ALTER TABLE … DISABLE TRIGGER ALL`
                // / `… ENABLE TRIGGER ALL`. Toggle the enabled
                // flag on every matching trigger so the row-
                // write paths skip them; the catalog snapshot
                // persists the new state across restarts.
                let table_name = s.name.to_string();
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
            }
            spg_sql::ast::AlterTableTarget::SetColumnAutoIncrement { column, seq_name } => {
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
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
                })?;
                let pos = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&column))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "ALTER COLUMN {column:?}: no such column on {:?}",
                            s.name
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
            }
            spg_sql::ast::AlterTableTarget::RenameTable { new } => {
                // v7.16.2 — table-level rename (mailrs round-10
                // A.5 — used by migrate-042's `ALTER TABLE
                // contacts RENAME TO email_contacts`). Storage
                // helper updates the schema + by_name index +
                // dangling FK / trigger references in one
                // atomic step.
                let old = s.name.to_string();
                self.active_catalog_mut()
                    .rename_table(&old, &new)
                    .map_err(EngineError::Storage)?;
            }
            spg_sql::ast::AlterTableTarget::RenameColumn { old, new } => {
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
                let table = self.active_catalog_mut().get_mut(s.name).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: s.name.into(),
                    })
                })?;
                let col_pos = table
                    .schema()
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&old))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "ALTER TABLE RENAME COLUMN: column {old:?} not found on {:?}",
                            s.name
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
                        s.name
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
                let table_name = s.name.to_string();
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
            }
        }
        Ok(())
    }

    fn exec_alter_index(
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

    fn exec_create_index(
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
    fn exec_drop_table(
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
    fn exec_drop_index(
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

    fn exec_create_table(
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
        // v7.9.19 — table-level constraints: PRIMARY KEY (a, b, ...)
        // and UNIQUE (a, b, ...). Each builds a BTree index on the
        // leading column (the existing single-column storage tier)
        // and registers a UniquenessConstraint on the schema for
        // INSERT-time enforcement of the full tuple. mailrs G1/G6.
        let cols = stmt
            .columns
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
        for tc in &stmt.table_constraints {
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
            Vec::with_capacity(stmt.foreign_keys.len());
        for fk in stmt.foreign_keys {
            // v7.14.0 — when SET FOREIGN_KEY_CHECKS=0 is in effect
            // (mysqldump preamble + bulk imports), defer FK
            // resolution if the parent table isn't in the catalog
            // yet. The FK is queued and resolved when checks flip
            // back on. Self-references stay in-band (the parent is
            // the same as the child we're building).
            let needs_parent = !fk.parent_table.eq_ignore_ascii_case(&table_name);
            if !self.foreign_key_checks
                && needs_parent
                && self.active_catalog().get(&fk.parent_table).is_none()
            {
                self.pending_foreign_keys.push((table_name.clone(), fk));
                continue;
            }
            fks.push(resolve_foreign_key(
                &table_name,
                &cols,
                fk,
                self.active_catalog(),
            )?);
        }
        let mut schema = TableSchema::new(table_name.clone(), cols);
        schema.foreign_keys = fks;
        // v7.9.19 — translate AST table_constraints to storage
        // UniquenessConstraints (column name → position) so the
        // INSERT enforcement helper sees positions directly.
        let mut uc_storage: Vec<spg_storage::UniquenessConstraint> = Vec::new();
        let mut check_exprs: Vec<String> = Vec::new();
        for tc in &stmt.table_constraints {
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
            for n in &inline_pk_columns {
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
        self.active_catalog_mut().create_table(schema)?;
        // v7.9.13 — implicit BTree per inline PK column +
        // v7.9.19 — implicit BTree on the leading column of every
        // table-level PRIMARY KEY / UNIQUE constraint.
        let table = self
            .active_catalog_mut()
            .get_mut(&table_name)
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
        for (i, tc) in stmt.table_constraints.iter().enumerate() {
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
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_insert(&mut self, mut stmt: InsertStatement) -> Result<QueryResult, EngineError> {
        // v7.17.0 Phase 1.1 — pre-resolve any nextval / currval /
        // setval calls against the catalog before the row loop. We
        // walk each tuple expression and replace matching
        // FunctionCall nodes with their concrete Literal. This
        // keeps `literal_expr_to_value` free of `&mut self` and
        // lets multi-row INSERT VALUES (… nextval('seq') …)
        // mint a separate sequence value per row.
        for tuple in &mut stmt.rows {
            for cell in tuple.iter_mut() {
                self.resolve_sequence_calls_in_expr(cell)?;
            }
        }
        // v7.13.0 — `INSERT INTO t [(cols)] SELECT …` (mailrs
        // round-5 G4). Execute the inner SELECT first, then route
        // back through the regular VALUES code path with the
        // materialised rows.
        if let Some(select) = stmt.select_source.clone() {
            let select_result = self.exec_select_cancel(&select, CancelToken::none())?;
            let rows = match select_result {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "INSERT … SELECT: inner statement produced {other:?} instead of a row set"
                    )));
                }
            };
            let mut materialised: Vec<Vec<Expr>> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut tuple: Vec<Expr> = Vec::with_capacity(row.values.len());
                for v in row.values {
                    tuple.push(value_to_literal_expr_permissive(v)?);
                }
                materialised.push(tuple);
            }
            let recurse = InsertStatement {
                table: stmt.table,
                columns: stmt.columns,
                rows: materialised,
                select_source: None,
                on_conflict: stmt.on_conflict,
                returning: stmt.returning,
            };
            return self.exec_insert(recurse);
        }
        // v7.9.21 — snapshot the clock fn pointer before the mut
        // borrow on the catalog opens; runtime DEFAULT eval needs
        // it inside the row hot loop.
        let clock = self.clock;
        // v7.12.4 — snapshot row-level triggers + their referenced
        // functions before the mut borrow on the catalog opens.
        // Cloned out so the row hot loop can fire them without
        // re-borrowing the catalog (which would conflict with
        // table.insert's mutable borrow).
        let before_insert_triggers = self.snapshot_row_triggers(&stmt.table, "INSERT", "BEFORE");
        let after_insert_triggers = self.snapshot_row_triggers(&stmt.table, "INSERT", "AFTER");
        let trigger_session_cfg: Option<alloc::string::String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v7.17.0 Phase 1.4 — snapshot the enum label lookup BEFORE
        // opening the mutable borrow on the table below. We need
        // catalog-level read access (enum_types lives at the
        // catalog level, not the table) and the upcoming mutable
        // borrow shadows it.
        let pre_borrow_column_meta: Vec<ColumnSchema> = {
            let preview_table = self.active_catalog().get(&stmt.table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
            preview_table.schema().columns.clone()
        };
        let enum_label_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM
                    // variant lists take priority over the PG
                    // catalog enum_types lookup (they're
                    // column-local and authoritative when set).
                    if let Some(inline) = &col.inline_enum_variants {
                        return Some((i, inline.clone()));
                    }
                    col.user_enum_type.as_ref().and_then(|ename| {
                        self.active_catalog()
                            .enum_types()
                            .get(ename)
                            .map(|e| (i, e.labels.clone()))
                    })
                })
                .collect();
        // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant lists.
        // Distinct from enum_label_lookup: SET validates that
        // every comma-separated token is in the variant list, and
        // canonicalises the cell to definition-order de-duped text.
        let set_variant_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| col.inline_set_variants.as_ref().map(|vs| (i, vs.clone())))
                .collect();
        // v7.29 (round-23a) - when the column's implicit sequence
        // exists (born on first nextval/setval address), a setval
        // above the table MAX moves the next auto-assigned id:
        // assign from max(table_max + 1, last_value + 1). Tables
        // whose sequence was never addressed keep the bare max+1
        // path (identical pre-7.29 behaviour, no lookup cost
        // beyond one map probe per auto column per statement).
        let mut seq_floors: alloc::collections::BTreeMap<usize, i64> =
            alloc::collections::BTreeMap::new();
        for (i, col) in pre_borrow_column_meta.iter().enumerate() {
            if col.auto_increment
                && let Some(sd) = self.active_catalog().sequences().get(&alloc::format!(
                    "{}_{}_seq",
                    stmt.table,
                    col.name
                ))
            {
                // is_called=false (fresh RESTART / setval(_, false))
                // means the NEXT value is last_value itself.
                let floor = if sd.is_called {
                    sd.last_value + 1
                } else {
                    sd.last_value
                };
                seq_floors.insert(i, floor);
            }
        }
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v3.1.5: clone the columns vector only (not the whole
        // TableSchema — saves one String alloc for the table name).
        // We need an owned snapshot because we'll call `table.insert`
        // (mutable borrow on `table`) inside the row loop while
        // reading schema fields.
        let column_meta: Vec<ColumnSchema> = table.schema().columns.clone();
        let schema_cols_len = column_meta.len();
        // Build a permutation `tuple_pos[c] = Some(j)` meaning schema
        // column `c` is filled from the `j`-th tuple slot; `None` means
        // "fill with NULL". Validated once and reused for every row.
        let tuple_pos: Option<Vec<Option<usize>>> = match &stmt.columns {
            None => None, // 1-1 mapping, fast path
            Some(cols) => {
                let mut map = alloc::vec![None; schema_cols_len];
                for (j, name) in cols.iter().enumerate() {
                    let idx = column_meta
                        .iter()
                        .position(|c| c.name == *name)
                        .ok_or_else(|| {
                            EngineError::Eval(EvalError::ColumnNotFound { name: name.clone() })
                        })?;
                    if map[idx].is_some() {
                        return Err(EngineError::Storage(StorageError::ArityMismatch {
                            expected: schema_cols_len,
                            actual: cols.len(),
                        }));
                    }
                    map[idx] = Some(j);
                }
                // Omitted columns must either be nullable, carry a
                // DEFAULT, or be AUTO_INCREMENT. Catch NOT NULL
                // omissions up front so the WAL stays clean.
                for (i, col) in column_meta.iter().enumerate() {
                    if map[i].is_none()
                        && !col.nullable
                        && col.default.is_none()
                        && col.runtime_default.is_none()
                        && !col.auto_increment
                    {
                        return Err(EngineError::Storage(StorageError::NullInNotNull {
                            column: col.name.clone(),
                        }));
                    }
                }
                Some(map)
            }
        };
        let expected_tuple_len = stmt.columns.as_ref().map_or(schema_cols_len, Vec::len);
        // v7.6.2 — snapshot this table's FK list before the
        // mutable-borrow window so we can run parent lookups
        // against the immutable catalog after parsing. Empty vec is
        // the no-FK fast path; clone cost is O(fks * arity) which
        // is < 100 ns for typical schemas.
        let fks = table.schema().foreign_keys.clone();
        let mut affected = 0usize;
        // Stage 1 — parse + AUTO_INC + coerce all rows under the
        // single mutable borrow.
        let mut all_values: Vec<Vec<Value>> = Vec::with_capacity(stmt.rows.len());
        // v7.24 (round-16 collateral) — statement-scoped serial
        // cursors. next_auto_value() is a max+1 scan over COMMITTED
        // rows; multi-row `INSERT … VALUES (…),(…)` computed it per
        // tuple BEFORE any insertion, so every row drew the SAME id
        // (then sailed through, compounding with the inline-PK
        // enforcement gap). First use per column seeds from the
        // table; subsequent rows increment.
        let mut auto_cursors: alloc::collections::BTreeMap<usize, i64> =
            alloc::collections::BTreeMap::new();
        for tuple in stmt.rows {
            if tuple.len() != expected_tuple_len {
                return Err(EngineError::Storage(StorageError::ArityMismatch {
                    expected: expected_tuple_len,
                    actual: tuple.len(),
                }));
            }
            // Fast path: no column-list permutation → tuple slot j
            // maps to schema column j. We can zip schema with tuple
            // and skip the `raw_tuple` staging allocation entirely.
            let values: Vec<Value> = if let Some(map) = &tuple_pos {
                // Permuted path: still need raw_tuple to index by `map[i]`.
                let raw_tuple: Vec<Value> = tuple
                    .into_iter()
                    .map(literal_expr_to_value)
                    .collect::<Result<_, _>>()?;
                let mut out = Vec::with_capacity(schema_cols_len);
                for (i, col) in column_meta.iter().enumerate() {
                    let mut raw = match map[i] {
                        Some(j) => raw_tuple[j].clone(),
                        None => resolve_column_default_free(col, clock)?,
                    };
                    if col.auto_increment && raw.is_null() {
                        let next = match auto_cursors.get(&i) {
                            Some(n) => *n,
                            None => {
                                let base = table.next_auto_value(i).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                        col.name
                                    ))
                                })?;
                                base.max(seq_floors.get(&i).copied().unwrap_or(i64::MIN))
                            }
                        };
                        auto_cursors.insert(i, next + 1);
                        raw = Value::BigInt(next);
                    }
                    let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                    enforce_enum_label(&enum_label_lookup, i, &col.name, &coerced)?;
                    let coerced =
                        canonicalize_set_value(&set_variant_lookup, i, &col.name, coerced)?;
                    check_unsigned_range(&coerced, col, i)?;
                    out.push(coerced);
                }
                out
            } else {
                // 1-1 mapping fast path: single Vec alloc, no raw_tuple.
                let mut out = Vec::with_capacity(schema_cols_len);
                for (i, (col, expr)) in column_meta.iter().zip(tuple).enumerate() {
                    let mut raw = literal_expr_to_value(expr)?;
                    if col.auto_increment && raw.is_null() {
                        let next = match auto_cursors.get(&i) {
                            Some(n) => *n,
                            None => {
                                let base = table.next_auto_value(i).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                        col.name
                                    ))
                                })?;
                                base.max(seq_floors.get(&i).copied().unwrap_or(i64::MIN))
                            }
                        };
                        auto_cursors.insert(i, next + 1);
                        raw = Value::BigInt(next);
                    }
                    let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                    enforce_enum_label(&enum_label_lookup, i, &col.name, &coerced)?;
                    let coerced =
                        canonicalize_set_value(&set_variant_lookup, i, &col.name, coerced)?;
                    check_unsigned_range(&coerced, col, i)?;
                    out.push(coerced);
                }
                out
            };
            all_values.push(values);
        }
        // Stage 2 — FK enforcement on the immutable catalog.
        // Non-lexical lifetimes release the mutable borrow on
        // `table` here since stage 1 was the last use. The
        // parent-table lookup runs before any row is committed.
        let uniqueness = table.schema().uniqueness_constraints.clone();
        let _ = table;
        if !fks.is_empty() {
            enforce_fk_inserts(self.active_catalog(), &stmt.table, &fks, &all_values)?;
        }
        // v7.13.0 — CHECK constraint enforcement (mailrs round-5 G3).
        enforce_check_constraints(self.active_catalog(), &stmt.table, &all_values)?;
        // NOTE (mailrs embed round-12): UNIQUE / PRIMARY KEY and
        // UNIQUE INDEX enforcement moved BELOW the ON CONFLICT
        // resolution pass. Running them first made every
        // `ON CONFLICT … DO UPDATE` upsert fail with a uniqueness
        // violation before the conflict handler could route the row
        // to an UPDATE — PG resolves the conflict action first and
        // only errors on rows no arbiter matched.
        // v7.9.8 / v7.9.9 — ON CONFLICT handling.
        //   - `DO NOTHING` filters `all_values` to non-conflicting
        //     rows + drops within-batch duplicates.
        //   - `DO UPDATE SET …` ALSO filters, but for each
        //     conflicting row it queues an UPDATE on the existing
        //     row using the incoming row's values as `EXCLUDED.*`.
        let mut pending_updates: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut skipped_count = 0usize;
        if let Some(clause) = &stmt.on_conflict {
            let (conflict_cols, conflict_nnd) = resolve_on_conflict_columns(
                self.active_catalog(),
                &stmt.table,
                clause.target_columns.as_slice(),
            )?;
            let mut kept: Vec<Vec<Value>> = Vec::with_capacity(all_values.len());
            let mut seen_keys: Vec<Vec<Value>> = Vec::new();
            for values in all_values {
                let key_tuple: Vec<&Value> = conflict_cols.iter().map(|&c| &values[c]).collect();
                // SQL spec: NULL in any conflict column means "no
                // conflict possible" (NULL ≠ NULL for uniqueness) —
                // UNLESS the constraint says NULLS NOT DISTINCT
                // (v7.29; mailrs migrate-013 replays its seed row
                // ('super', NULL) under exactly that declaration).
                let has_null_key =
                    !conflict_nnd && key_tuple.iter().any(|v| matches!(v, Value::Null));
                let collides_with_table = !has_null_key
                    && on_conflict_keys_exist(
                        self.active_catalog(),
                        &stmt.table,
                        &conflict_cols,
                        &key_tuple,
                    );
                let key_tuple_owned: Vec<Value> = key_tuple.iter().map(|v| (*v).clone()).collect();
                let collides_with_batch =
                    !has_null_key && seen_keys.iter().any(|k| k == &key_tuple_owned);
                let collides = collides_with_table || collides_with_batch;
                match (&clause.action, collides) {
                    (_, false) => {
                        seen_keys.push(key_tuple_owned);
                        kept.push(values);
                    }
                    (spg_sql::ast::OnConflictAction::Nothing, true) => {
                        skipped_count += 1;
                    }
                    (
                        spg_sql::ast::OnConflictAction::Update {
                            assignments,
                            where_,
                        },
                        true,
                    ) => {
                        if !collides_with_table {
                            skipped_count += 1;
                            continue;
                        }
                        let target_pos = lookup_row_position_by_keys(
                            self.active_catalog(),
                            &stmt.table,
                            &conflict_cols,
                            &key_tuple,
                        )
                        .ok_or_else(|| {
                            EngineError::Unsupported(
                                "ON CONFLICT DO UPDATE: conflict detected but row \
                                 position could not be resolved (cold-tier row?)"
                                    .into(),
                            )
                        })?;
                        let updated = apply_on_conflict_assignments(
                            self.active_catalog(),
                            &stmt.table,
                            target_pos,
                            &values,
                            assignments,
                            where_.as_ref(),
                        )?;
                        if let Some(new_row) = updated {
                            pending_updates.push((target_pos, new_row));
                        } else {
                            skipped_count += 1;
                        }
                    }
                }
            }
            all_values = kept;
        }
        // v7.9.19 — composite UNIQUE / PRIMARY KEY enforcement.
        // v7.9.29 — CREATE UNIQUE INDEX [WHERE pred] enforcement.
        // Both run on the post-ON-CONFLICT row set: conflicting rows
        // already left `all_values` (DO NOTHING drop / DO UPDATE
        // reroute), so what remains must be genuinely unique.
        enforce_uniqueness_inserts(self.active_catalog(), &stmt.table, &uniqueness, &all_values)?;
        enforce_unique_index_inserts(self.active_catalog(), &stmt.table, &all_values)?;
        // Stage 3 — insert all rows under a fresh mutable borrow.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v7.9.4 — keep RETURNING projection rows separate per
        // INSERT and per UPDATE branch so DO UPDATE pushes the new
        // post-update state, not the incoming-only values.
        let mut returning_rows: Vec<Vec<Value>> = Vec::new();
        // v7.12.7 — collect embedded SQL emitted by any trigger
        // fire across the row loop; engine drains the queue after
        // the table mut borrow drops.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        'rowloop: for values in all_values {
            let mut row = Row::new(values);
            // v7.12.4 — BEFORE INSERT row-level triggers. Each
            // trigger may rewrite NEW cells (e.g. populate
            // `search_vector := to_tsvector(...)`) and may return
            // NULL to skip the row entirely.
            for fd in &before_insert_triggers {
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(row.clone()),
                    None,
                    &stmt.table,
                    &column_meta,
                    &[],
                    trigger_session_cfg.as_deref(),
                    false,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => row = r,
                    triggers::TriggerOutcome::Skip => continue 'rowloop,
                }
            }
            if stmt.returning.is_some() {
                returning_rows.push(row.values.clone());
            }
            // v7.12.4 — clone for the AFTER trigger view; insert
            // moves the row into the table.
            let inserted = row.clone();
            table.insert(row)?;
            affected += 1;
            // v7.12.4 — AFTER INSERT row-level triggers fire post-
            // write. Return value is ignored (PG semantics); we
            // surface any error from the body up to the caller.
            for fd in &after_insert_triggers {
                let (_outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(inserted.clone()),
                    None,
                    &stmt.table,
                    &column_meta,
                    &[],
                    trigger_session_cfg.as_deref(),
                    true,
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
            }
        }
        // v7.9.9 — apply ON CONFLICT DO UPDATE rewrites collected
        // in the conflict-resolution pass. update_row handles
        // index maintenance + body re-encoding.
        for (pos, new_row) in pending_updates {
            if stmt.returning.is_some() {
                returning_rows.push(new_row.clone());
            }
            table.update_row(pos, new_row)?;
            affected += 1;
        }
        let _ = skipped_count;
        // v7.12.7 — drop the table mut borrow and drain any
        // trigger-emitted embedded SQL queued during this INSERT.
        // The borrow has to release first because each deferred
        // stmt may UPDATE / INSERT / DELETE the same (or another)
        // table — including, in principle, this one.
        let _ = table;
        self.execute_deferred_trigger_stmts(deferred_embedded, CancelToken::none())?;
        // v7.9.4/v7.9.9 — RETURNING streams the rows that ended
        // up in the table after this statement (insert or
        // post-update on conflict).
        if let Some(items) = &stmt.returning {
            return self.build_returning_rows(&stmt.table, items, returning_rows);
        }
        // v6.2.1 — auto-analyze: track per-table modified-row
        // counter so the background sweep can decide when to
        // re-ANALYZE. Cheap path on the autocommit-wrap hot loop
        // — one BTreeMap entry update per INSERT batch.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.5: SELECT with cooperative cancellation. The token is
    /// honoured between UNION peers and inside the bare-SELECT row
    /// loop; HNSW kNN graph walks and the aggregate executor don't
    /// honour it yet (deferred — those paths bound their work
    /// internally by `LIMIT k` and `GROUP BY` cardinality).
    /// v6.10.2 — cold-tier time-travel scan. Resolves the segment
    /// by id, decodes each row body against the table's current
    /// schema, applies the SELECT's projection + optional WHERE +
    /// optional LIMIT, returns a `Rows` result. JOINs / aggregates
    /// / ORDER BY are unsupported on this path (STABILITY carve-
    /// out); operators wanting them should restore the segment
    /// into a regular table first.
    fn exec_select_as_of_segment(
        &self,
        stmt: &SelectStatement,
        from: &spg_sql::ast::FromClause,
        segment_id: u32,
    ) -> Result<QueryResult, EngineError> {
        // v6.10.2 scope: no joins, no aggregates, no ORDER BY,
        // no GROUP BY / HAVING / UNION / OFFSET / DISTINCT.
        if !from.joins.is_empty()
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.offset.is_some()
            || stmt.distinct
            || aggregate::uses_aggregate(stmt)
        {
            return Err(EngineError::Unsupported(
                "AS OF SEGMENT supports SELECT projection + WHERE + LIMIT only \
                 (joins / aggregates / ORDER BY are STABILITY § \"Out of v6.10\")"
                    .into(),
            ));
        }
        let table = self
            .active_catalog()
            .get(&from.primary.name)
            .ok_or_else(|| StorageError::TableNotFound {
                name: from.primary.name.clone(),
            })?;
        let schema = table.schema().clone();
        let schema_cols = &schema.columns;
        let alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let ctx = EvalContext::new(schema_cols, Some(alias));
        let seg = self
            .active_catalog()
            .cold_segment(segment_id)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "AS OF SEGMENT: cold segment {segment_id} not registered"
                ))
            })?;
        let mut out_rows: Vec<Row> = Vec::new();
        let mut limit_remaining: Option<usize> =
            stmt.limit_literal().and_then(|n| usize::try_from(n).ok());
        for (_key, body) in seg.scan() {
            let (row, _consumed) =
                spg_storage::decode_row_body_dense(&body, &schema, seg.codec_version())
                    .map_err(EngineError::Storage)?;
            if let Some(where_expr) = &stmt.where_ {
                let cond = self.eval_expr_simple(where_expr, &row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            // Projection.
            let projected = self.project_row_simple(&row, &stmt.items, schema_cols, alias)?;
            out_rows.push(projected);
            if let Some(rem) = limit_remaining.as_mut() {
                if *rem == 0 {
                    out_rows.pop();
                    break;
                }
                *rem -= 1;
            }
        }
        // Output column schema: derive from SELECT items.
        let columns = self.derive_output_columns(&stmt.items, schema_cols, alias);
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }

    /// v6.10.2 — simple-path WHERE eval that doesn't go through
    /// the correlated-subquery / Memoize machinery. AS OF SEGMENT
    /// scan paths predicate against a snapshot frozen segment, no
    /// cross-row state.
    fn eval_expr_simple(
        &self,
        expr: &Expr,
        row: &Row,
        ctx: &EvalContext,
    ) -> Result<Value, EngineError> {
        let cancel = CancelToken::none();
        self.eval_expr_with_correlated(expr, row, ctx, cancel, None)
    }

    /// v7.9.4 — INSERT / UPDATE / DELETE RETURNING projector.
    /// Given the table name, the user-supplied projection items,
    /// and the mutated rows (post-insert / post-update values, or
    /// pre-delete snapshot), build a `QueryResult::Rows` whose
    /// schema describes the projected columns. Mailrs migration
    /// blocker #1.
    fn build_returning_rows(
        &self,
        table_name: &str,
        items: &[SelectItem],
        mutated_rows: Vec<Vec<Value>>,
    ) -> Result<QueryResult, EngineError> {
        let table = self.active_catalog().get(table_name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: table_name.into(),
            })
        })?;
        let schema_cols = table.schema().columns.clone();
        let columns = self.derive_output_columns(items, &schema_cols, table_name);
        let mut out_rows: Vec<Row> = Vec::with_capacity(mutated_rows.len());
        for values in mutated_rows {
            let row = Row::new(values);
            let projected = self.project_row_simple(&row, items, &schema_cols, table_name)?;
            out_rows.push(projected);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }

    /// v6.10.2 — projection for AS OF SEGMENT. Resolves
    /// `SelectItem::Wildcard` to all schema columns and
    /// `SelectItem::Expr` via the regular eval path.
    fn project_row_simple(
        &self,
        row: &Row,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        alias: &str,
    ) -> Result<Row, EngineError> {
        let ctx = EvalContext::new(schema_cols, Some(alias));
        let cancel = CancelToken::none();
        let mut out_vals = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard => {
                    out_vals.extend(row.values.iter().cloned());
                }
                SelectItem::Expr { expr, .. } => {
                    let v = self.eval_expr_with_correlated(expr, row, &ctx, cancel, None)?;
                    out_vals.push(v);
                }
            }
        }
        Ok(Row::new(out_vals))
    }

    /// v6.10.2 — derive the output `ColumnSchema` list for an
    /// AS OF SEGMENT projection. Wildcards take the full schema;
    /// expressions take the alias if present or a synthetic
    /// `?column?` (PG convention) otherwise.
    fn derive_output_columns(
        &self,
        items: &[SelectItem],
        schema_cols: &[ColumnSchema],
        _alias: &str,
    ) -> Vec<ColumnSchema> {
        let mut out = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard => {
                    out.extend(schema_cols.iter().cloned());
                }
                SelectItem::Expr { expr, alias } => {
                    // Bare column references inherit the schema
                    // column's name + type — PG names `RETURNING id`
                    // "id" and types it BIGINT, and the sqlx embed
                    // path type-checks RowDescription against the
                    // Rust target (mailrs embed round-12).
                    if let Expr::Column(col) = expr
                        && let Some(sc) = schema_cols.iter().find(|c| c.name == col.name)
                    {
                        let name = alias.clone().unwrap_or_else(|| sc.name.clone());
                        out.push(ColumnSchema::new(name, sc.ty, sc.nullable));
                        continue;
                    }
                    let name = alias.clone().unwrap_or_else(|| "?column?".to_string());
                    // Default to Text; the caller's row values
                    // carry the actual type. v6.10.2 scope.
                    out.push(ColumnSchema::new(name, DataType::Text, true));
                }
            }
        }
        out
    }

    fn exec_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v7.17.0 Phase 1.2 — user-defined VIEW expansion. If the
        // FROM / JOIN graph references any catalogued view name,
        // re-parse the view body and prepend it as a synthetic
        // CTE. Recurses on views-in-views via the regular CTE
        // dispatch below. Fast-path: skip the walker entirely when
        // the catalog has no views (the typical OLTP load).
        if !self.active_catalog().views().is_empty() {
            if let Some(rewritten) = self.expand_views_in_select(stmt)? {
                return self.exec_select_cancel(&rewritten, cancel);
            }
        }
        // v7.16.2 — information_schema / pg_catalog virtual
        // views (mailrs round-10 A.3). If the SELECT touches a
        // synthetic meta-table name (`__spg_info_*` /
        // `__spg_pg_*` — produced by the parser for
        // `information_schema.X` / `pg_catalog.X`), clone the
        // catalog, materialise the requested view as a real
        // temporary table, and re-execute against an enriched
        // engine. Same pattern as `exec_with_ctes` for CTEs.
        if !self.meta_views_materialised && select_references_meta_view(stmt) {
            return self.exec_select_with_meta_views(stmt, cancel);
        }
        // v6.10.2 — cold-tier time-travel short-circuit. When the
        // primary TableRef carries `AS OF SEGMENT '<id>'`, run a
        // dedicated cold-segment scan instead of the regular
        // hot+index path. The scope is intentionally narrow for
        // v6.10.2 — bare `SELECT * FROM <t> AS OF SEGMENT 'id'`,
        // optionally with a single-column-equality WHERE. JOINs /
        // aggregates / ORDER BY / subqueries on top of a time-
        // travelled scan are STABILITY § "Out of v6.10".
        if let Some(from) = &stmt.from
            && let Some(seg_id) = from.primary.as_of_segment
        {
            return self.exec_select_as_of_segment(stmt, from, seg_id);
        }
        // v6.2.0 / v6.5.0 — virtual-table short-circuits. Detected
        // pre-CTE because they don't read from the catalog and
        // shouldn't participate in regular FROM resolution.
        if let Some(from) = &stmt.from
            && from.joins.is_empty()
            && stmt.where_.is_none()
            && stmt.group_by.is_none()
            && stmt.having.is_none()
            && stmt.unions.is_empty()
            && stmt.order_by.is_empty()
            && stmt.limit.is_none()
            && stmt.offset.is_none()
            && !stmt.distinct
            && stmt.items.iter().all(|i| matches!(i, SelectItem::Wildcard))
        {
            let lower = from.primary.name.to_ascii_lowercase();
            match lower.as_str() {
                "spg_statistic" => return Ok(self.exec_spg_statistic()),
                // v6.5.0 — observability v2 virtual tables.
                "spg_stat_replication" => return Ok(self.exec_spg_stat_replication()),
                "spg_stat_segment" => return Ok(self.exec_spg_stat_segment()),
                "spg_stat_query" => return Ok(self.exec_spg_stat_query()),
                "spg_stat_activity" => return Ok(self.exec_spg_stat_activity()),
                "spg_audit_chain" => return Ok(self.exec_spg_audit_chain()),
                "spg_audit_verify" => return Ok(self.exec_spg_audit_verify()),
                "spg_table_ddl" => return Ok(self.exec_spg_table_ddl()),
                "spg_role_ddl" => return Ok(self.exec_spg_role_ddl()),
                "spg_database_ddl" => return Ok(self.exec_spg_database_ddl()),
                _ => {}
            }
        }
        // v4.11: CTEs materialise into a temporary enriched catalog
        // *before* anything else — the body SELECT can then refer
        // to CTE names via the regular FROM-clause resolution.
        // Uncorrelated only: each CTE body runs once against the
        // current catalog, not against later CTEs' results (left-
        // to-right materialisation would relax this, but we keep
        // it simple for v4.11 MVP).
        if !stmt.ctes.is_empty() {
            return self.exec_with_ctes(stmt, cancel);
        }
        // v4.10: subqueries (uncorrelated) are resolved here, before
        // the executor sees the row loop. We clone the statement so
        // we can mutate without disturbing the caller's AST — most
        // queries pass through with no subquery nodes and the clone
        // is cheap; with subqueries the materialisation cost
        // dominates anyway.
        let mut stmt_owned;
        let stmt_ref: &SelectStatement = if expr_tree_has_subquery(stmt) {
            stmt_owned = stmt.clone();
            self.resolve_select_subqueries(&mut stmt_owned, cancel)?;
            &stmt_owned
        } else {
            stmt
        };
        if stmt_ref.unions.is_empty() {
            return self.exec_bare_select_cancel(stmt_ref, cancel);
        }
        // UNION path: clone-strip the head into a bare block (its own
        // DISTINCT and any inner ORDER BY are dropped by parser rule —
        // the wrapper SelectStatement carries them), execute, then chain
        // peers with left-associative dedup semantics.
        let mut head = stmt_ref.clone();
        head.unions = Vec::new();
        head.order_by = Vec::new();
        head.limit = None;
        let QueryResult::Rows { columns, mut rows } =
            self.exec_bare_select_cancel(&head, cancel)?
        else {
            unreachable!("bare SELECT cannot return CommandOk")
        };
        for (kind, peer) in &stmt_ref.unions {
            let QueryResult::Rows {
                columns: peer_cols,
                rows: peer_rows,
            } = self.exec_bare_select_cancel(peer, cancel)?
            else {
                unreachable!("bare SELECT cannot return CommandOk")
            };
            if peer_cols.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UNION arity mismatch: head has {} columns, peer has {}",
                    columns.len(),
                    peer_cols.len()
                )));
            }
            rows.extend(peer_rows);
            if matches!(kind, UnionKind::Distinct) {
                rows = dedup_rows(rows);
            }
        }
        // ORDER BY at the top of a UNION applies to the combined result.
        // Eval against the projected schema (NOT the source table).
        if !stmt.order_by.is_empty() {
            let synth_ctx = EvalContext::new(&columns, None);
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            let mut tagged: Vec<(Vec<f64>, Row)> = Vec::with_capacity(rows.len());
            for r in rows {
                let keys = build_order_keys(&stmt.order_by, &r, &synth_ctx)?;
                tagged.push((keys, r));
            }
            sort_by_keys(&mut tagged, &descs);
            rows = tagged.into_iter().map(|(_, r)| r).collect();
        }
        apply_offset_and_limit(&mut rows, stmt.offset_literal(), stmt.limit_literal());
        Ok(QueryResult::Rows { columns, rows })
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_lines)] // huge match — splitting fragments the planner
    /// v7.11.7 — execute `SELECT … FROM unnest(expr) [AS] alias …`.
    /// Synthesises a single-column virtual table whose column type
    /// is TEXT and whose rows are the array elements. Routes
    /// through the regular projection / WHERE / ORDER BY / LIMIT
    /// machinery so set-returning UNNEST composes naturally with
    /// the rest of the SELECT surface.
    fn exec_select_unnest(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let expr = primary
            .unnest_expr
            .as_deref()
            .expect("caller guards unnest_expr.is_some()");
        // Evaluate the array expression once. Empty schema / empty
        // row — uncorrelated UNNEST cannot reference outer columns.
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        // v7.11.13 — unnest dispatches per array element type so
        // INT[] / BIGINT[] surface their PG types in projection.
        let (elem_dtype, rows): (DataType, alloc::vec::Vec<Row>) =
            match eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)? {
                Value::Null => (DataType::Text, alloc::vec::Vec::new()),
                Value::TextArray(items) => {
                    let rows = items
                        .into_iter()
                        .map(|item| {
                            Row::new(alloc::vec![match item {
                                Some(s) => Value::Text(s),
                                None => Value::Null,
                            }])
                        })
                        .collect();
                    (DataType::Text, rows)
                }
                Value::IntArray(items) => {
                    let rows = items
                        .into_iter()
                        .map(|item| {
                            Row::new(alloc::vec![match item {
                                Some(n) => Value::Int(n),
                                None => Value::Null,
                            }])
                        })
                        .collect();
                    (DataType::Int, rows)
                }
                Value::BigIntArray(items) => {
                    let rows = items
                        .into_iter()
                        .map(|item| {
                            Row::new(alloc::vec![match item {
                                Some(n) => Value::BigInt(n),
                                None => Value::Null,
                            }])
                        })
                        .collect();
                    (DataType::BigInt, rows)
                }
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "unnest() expects an array argument, got {:?}",
                        other.data_type()
                    )));
                }
            };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "unnest".to_string());
        // v7.13.2 — mailrs round-6 S5. Honour PG-standard
        // `UNNEST(arr) AS p(col_name)` column-list aliasing: the
        // first entry overrides the projected column's name.
        // Without the column list, fall back to the table alias
        // (pre-v7.13.2 behaviour).
        let col_name = primary
            .unnest_column_aliases
            .first()
            .cloned()
            .unwrap_or_else(|| alias.clone());
        let col_schema = ColumnSchema::new(col_name, elem_dtype, true);
        let schema_cols = alloc::vec![col_schema.clone()];
        let scan_ctx = EvalContext::new(&schema_cols, Some(&alias));
        // Apply WHERE.
        let filtered: alloc::vec::Vec<Row> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = eval::eval_expr(w, &row, &scan_ctx).map_err(EngineError::Eval)?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // v7.17.0 Phase 3.P0-48 — aggregate dispatch over the
        // unnest source. Same routing the relational scan path
        // already takes — without it `SELECT COUNT(*) FROM
        // unnest(ARRAY[…])` either errored at projection time or
        // returned the wrong shape.
        if aggregate::uses_aggregate(stmt) {
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let filtered_refs: alloc::vec::Vec<&Row> = filtered.iter().collect();
            let mut agg = aggregate::run(
                stmt,
                &filtered_refs,
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
            )?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset_literal(), stmt.limit_literal());
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }
        // Projection.
        let projection = build_projection(&stmt.items, &schema_cols, &alias)?;
        let mut projected_rows: alloc::vec::Vec<Row> =
            alloc::vec::Vec::with_capacity(filtered.len());
        // v7.19 P5 — Set-Returning-Function in projection
        // position (PG `SELECT unnest(arr) FROM t` shape). When a
        // SELECT item evaluates to a top-level unnest(arr) call,
        // expand it: for each input row, evaluate the array, emit
        // one output row per element, broadcasting non-SRF
        // projections from the same input row. Multi-SRF + LCM
        // padding stays a documented carve-out; mailrs uses
        // single-SRF for redirect_uris.
        let srf_position = projection.iter().position(|p| is_top_level_unnest(&p.expr));
        if let Some(srf_idx) = srf_position {
            let srf_arg = top_level_unnest_arg(&projection[srf_idx].expr)
                .expect("checked by is_top_level_unnest above");
            for row in &filtered {
                let arr_val =
                    eval::eval_expr(srf_arg, row, &scan_ctx).map_err(EngineError::Eval)?;
                let elements = array_value_to_elements(&arr_val)?;
                // Empty array → zero rows for this input row (PG
                // semantics: `SELECT unnest('{}'::int[])` returns
                // 0 rows, not a single NULL row).
                for elem in elements {
                    let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                    for (i, p) in projection.iter().enumerate() {
                        if i == srf_idx {
                            vals.push(elem.clone());
                        } else {
                            vals.push(
                                eval::eval_expr(&p.expr, row, &scan_ctx)
                                    .map_err(EngineError::Eval)?,
                            );
                        }
                    }
                    projected_rows.push(Row::new(vals));
                }
            }
        } else {
            // v7.24 (round-16 B) — select-list subqueries resolve
            // per row (correlated-aware; plain exprs take the fast
            // path inside).
            let mut proj_memo = memoize::MemoizeCache::default();
            for row in &filtered {
                let mut vals = alloc::vec::Vec::with_capacity(projection.len());
                for p in &projection {
                    vals.push(self.eval_expr_with_correlated(
                        &p.expr,
                        row,
                        &scan_ctx,
                        cancel,
                        Some(&mut proj_memo),
                    )?);
                }
                projected_rows.push(Row::new(vals));
            }
        }
        // ORDER BY / LIMIT — apply on the projected rows (cheap;
        // unnest result sets are small by design).
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();
        // Re-evaluate ORDER BY against the source schema (pre-projection
        // so col refs by name still resolve through `scan_ctx`).
        if !stmt.order_by.is_empty() {
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value>)> = filtered
                .iter()
                .enumerate()
                .map(|(i, r)| -> Result<_, EngineError> {
                    let keys: Result<Vec<Value>, EngineError> = stmt
                        .order_by
                        .iter()
                        .map(|ob| {
                            eval::eval_expr(&ob.expr, r, &scan_ctx).map_err(EngineError::Eval)
                        })
                        .collect();
                    Ok((i, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &stmt.order_by[idx];
                    let cmp = order_by_value_cmp(o.desc, o.nulls_first, ka, kb);
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        // LIMIT / OFFSET — apply at the tail.
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    /// v7.17.0 Phase 3.10 — `FROM generate_series(start, stop [,
    /// step])` set-returning source. Mirrors `exec_select_unnest`'s
    /// shape: evaluate the arg list once against an empty row,
    /// materialise the row stream by stepping start → stop, then
    /// route through the standard WHERE / projection / ORDER BY /
    /// LIMIT pipeline. Two arg-type combos in v7.17:
    ///   * integer / integer [/ integer] — SmallInt, Int, BigInt
    ///     (widened to BigInt internally; step defaults to 1)
    ///   * timestamp / timestamp / interval — date-range
    ///     iteration (mailrs's daily-report pattern)
    fn exec_select_generate_series(
        &self,
        stmt: &SelectStatement,
        primary: &TableRef,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let args = primary
            .generate_series_args
            .as_ref()
            .expect("caller guards generate_series_args.is_some()");
        let empty_schema: alloc::vec::Vec<ColumnSchema> = alloc::vec::Vec::new();
        let ctx = EvalContext::new(&empty_schema, None);
        let dummy_row = Row::new(alloc::vec::Vec::new());
        let mut arg_values: alloc::vec::Vec<Value> = alloc::vec::Vec::with_capacity(args.len());
        for a in args {
            arg_values.push(eval::eval_expr(a, &dummy_row, &ctx).map_err(EngineError::Eval)?);
        }
        // Dispatch on the start value's shape. Reject mixed-shape
        // calls early (e.g. start = timestamp, stop = integer) so
        // the caller gets a clean error rather than a panic.
        let (elem_dtype, rows) = match arg_values.as_slice() {
            [Value::Timestamp(start), Value::Timestamp(stop), step] => {
                let interval_step = match step {
                    Value::Interval { .. } => step.clone(),
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "generate_series(timestamp, timestamp, …): \
                             step must be INTERVAL, got {:?}",
                            other.data_type()
                        )));
                    }
                };
                let rows = generate_series_timestamps(*start, *stop, interval_step, &cancel)?;
                (DataType::Timestamp, rows)
            }
            [start, stop, step]
                if value_is_integer(start) && value_is_integer(stop) && value_is_integer(step) =>
            {
                let s = value_to_i64(start);
                let e = value_to_i64(stop);
                let st = value_to_i64(step);
                let rows = generate_series_integers(s, e, st, &cancel)?;
                (DataType::BigInt, rows)
            }
            [start, stop] if value_is_integer(start) && value_is_integer(stop) => {
                let s = value_to_i64(start);
                let e = value_to_i64(stop);
                let rows = generate_series_integers(s, e, 1, &cancel)?;
                (DataType::BigInt, rows)
            }
            _ => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "generate_series(): v7.17 supports integer or (timestamp, timestamp, interval) \
                     argument shapes; got {:?}",
                    arg_values
                        .iter()
                        .map(|v| v.data_type())
                        .collect::<alloc::vec::Vec<_>>()
                )));
            }
        };
        let alias = primary
            .alias
            .clone()
            .unwrap_or_else(|| "generate_series".to_string());
        let col_name = alias.clone();
        let col_schema = ColumnSchema::new(col_name, elem_dtype, true);
        let schema_cols = alloc::vec![col_schema.clone()];
        let scan_ctx = EvalContext::new(&schema_cols, Some(&alias));
        // WHERE.
        let filtered: alloc::vec::Vec<Row> = if let Some(w) = &stmt.where_ {
            let mut out = alloc::vec::Vec::with_capacity(rows.len());
            for row in rows {
                cancel.check()?;
                let v = eval::eval_expr(w, &row, &scan_ctx).map_err(EngineError::Eval)?;
                if matches!(v, Value::Bool(true)) {
                    out.push(row);
                }
            }
            out
        } else {
            rows
        };
        // v7.17.0 Phase 3.P0-48 — aggregate dispatch for set-
        // returning sources. When the SELECT projection contains
        // aggregate functions (COUNT/SUM/MIN/MAX/AVG/string_agg/
        // …) we route the filtered row stream through the same
        // aggregate executor the relational scan path uses, so
        // `SELECT COUNT(*) FROM generate_series(1, 100)` returns
        // a single 100 row instead of erroring at projection
        // time. GROUP BY / HAVING / ORDER BY over the aggregate
        // output all ride through `aggregate::run`.
        if aggregate::uses_aggregate(stmt) {
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let filtered_refs: alloc::vec::Vec<&Row> = filtered.iter().collect();
            let mut agg = aggregate::run(
                stmt,
                &filtered_refs,
                &schema_cols,
                Some(&alias),
                Some(&agg_correlated),
            )?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset_literal(), stmt.limit_literal());
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }
        // Projection.
        let projection = build_projection(&stmt.items, &schema_cols, &alias)?;
        let mut projected_rows: alloc::vec::Vec<Row> =
            alloc::vec::Vec::with_capacity(filtered.len());
        let mut proj_memo = memoize::MemoizeCache::default();
        for row in &filtered {
            let mut vals = alloc::vec::Vec::with_capacity(projection.len());
            for p in &projection {
                // v7.24 (round-16 B) — correlated-aware.
                vals.push(self.eval_expr_with_correlated(
                    &p.expr,
                    row,
                    &scan_ctx,
                    cancel,
                    Some(&mut proj_memo),
                )?);
            }
            projected_rows.push(Row::new(vals));
        }
        let columns: alloc::vec::Vec<ColumnSchema> = projection
            .iter()
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();
        // ORDER BY against the source schema.
        if !stmt.order_by.is_empty() {
            let mut indexed: alloc::vec::Vec<(usize, Vec<Value>)> = filtered
                .iter()
                .enumerate()
                .map(|(i, r)| -> Result<_, EngineError> {
                    let keys: Result<Vec<Value>, EngineError> = stmt
                        .order_by
                        .iter()
                        .map(|ob| {
                            eval::eval_expr(&ob.expr, r, &scan_ctx).map_err(EngineError::Eval)
                        })
                        .collect();
                    Ok((i, keys?))
                })
                .collect::<Result<_, _>>()?;
            indexed.sort_by(|a, b| {
                for (idx, (ka, kb)) in a.1.iter().zip(b.1.iter()).enumerate() {
                    let o = &stmt.order_by[idx];
                    let cmp = order_by_value_cmp(o.desc, o.nulls_first, ka, kb);
                    if cmp != core::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                core::cmp::Ordering::Equal
            });
            projected_rows = indexed
                .into_iter()
                .map(|(i, _)| projected_rows[i].clone())
                .collect();
        }
        if let Some(offset) = stmt.offset_literal() {
            let off = (offset as usize).min(projected_rows.len());
            projected_rows.drain(..off);
        }
        if let Some(limit) = stmt.limit_literal() {
            projected_rows.truncate(limit as usize);
        }
        Ok(QueryResult::Rows {
            columns,
            rows: projected_rows,
        })
    }

    fn exec_bare_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.17.0 Phase 3.P0-49 — `FETCH FIRST N ROWS WITH TIES`
        // is meaningless without an ORDER BY; PG raises a hard
        // error and SPG mirrors the surface so the same DDL/app
        // path behaves identically on cutover.
        check_with_ties_requires_order_by(stmt)?;
        // v7.16.2 — same meta-view dispatch as
        // `exec_select_cancel`, applied here too because
        // `subquery_replacement` enters this function directly
        // for Exists / ScalarSubquery / InSubquery resolution
        // (bypassing the top-level entry to avoid double
        // subquery walking). Without this dispatch the subquery
        // hits `__spg_info_columns` and reports TableNotFound.
        if !self.meta_views_materialised && select_references_meta_view(stmt) {
            return self.exec_select_with_meta_views(stmt, cancel);
        }
        // v4.12: window-function path. When the projection contains
        // any `name(args) OVER (...)` we route to the dedicated
        // executor — partition + sort + per-row window value before
        // the regular projection.
        if select_has_window(stmt) {
            return self.exec_select_with_window(stmt, cancel);
        }
        // Constant SELECT (no FROM) — evaluate each item once against an
        // empty dummy row. Useful for `SELECT 1`, `SELECT coalesce(...)`,
        // `SELECT '7'::INT`. Column references will surface as
        // ColumnNotFound on eval since the schema is empty.
        let Some(from) = &stmt.from else {
            let empty_schema: Vec<ColumnSchema> = Vec::new();
            let ctx = self.ev_ctx(&empty_schema, None);
            let projection = build_projection(&stmt.items, &empty_schema, "")?;
            let dummy_row = Row::new(Vec::new());
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, &dummy_row, &ctx)?);
            }
            let columns: Vec<ColumnSchema> = projection
                .into_iter()
                .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
                .collect();
            return Ok(QueryResult::Rows {
                columns,
                rows: alloc::vec![Row::new(values)],
            });
        };
        // Multi-table FROM (one or more joined peers) goes through the
        // nested-loop join executor. Single-table FROM stays on the
        // existing scan + index-seek path.
        if !from.joins.is_empty() {
            return self.exec_joined_select(stmt, from, cancel);
        }
        // v7.11.7 — `FROM unnest(<expr>) [AS] <alias>`. Synthesise a
        // single-column table at SELECT entry by evaluating the
        // expression once against the empty row (UNNEST is
        // uncorrelated in v7.11; correlated / LATERAL unnest is a
        // v7.12 carve-out). Build a virtual `Table` in a heap-only
        // catalog, then route to the regular scan path.
        if from.primary.unnest_expr.is_some() {
            return self.exec_select_unnest(stmt, &from.primary, cancel);
        }
        // v7.17.0 Phase 3.10 — `FROM generate_series(start, stop
        // [, step])` set-returning source. Dispatch mirrors UNNEST:
        // materialise the row stream from a single eval pass, then
        // run the regular projection / WHERE / ORDER BY / LIMIT
        // pipeline over the synthetic single-column table.
        if from.primary.generate_series_args.is_some() {
            return self.exec_select_generate_series(stmt, &from.primary, cancel);
        }
        let primary = &from.primary;
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let schema_cols = &table.schema().columns;
        // The qualifier accepted on column refs is the alias (if any) else the
        // bare table name.
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let ctx = self.ev_ctx(schema_cols, Some(alias));

        // NSW kNN planner: `ORDER BY col <-> literal LIMIT k` with no
        // WHERE and an NSW index on `col` skips the full scan. The
        // walk returns rows already in ascending-distance order, so
        // ORDER BY / LIMIT are honoured implicitly.
        if let Some(nsw_rows) = try_nsw_knn(stmt, table, schema_cols, alias) {
            return materialise_in_order(stmt, table, schema_cols, alias, &nsw_rows);
        }

        // Index seek: if WHERE is `col = literal` (or commuted) and the
        // referenced column has an index, dispatch each locator through
        // the catalog (hot tier → borrow, cold tier → page-read +
        // decode) and iterate just those rows. Otherwise fall back to a
        // full scan over the hot tier (cold-tier rows are only reached
        // via index seek in v5.1 — full table scans against cold-tier
        // data ship in v5.2 with the freezer's per-segment scan API).
        let indexed_rows: Option<Vec<Cow<'_, Row>>> = stmt.where_.as_ref().and_then(|w| {
            // BTree / col=literal seek first — covers the v7.11.3 multi-
            // column AND case and the leading-column equality lookup.
            try_index_seek(w, schema_cols, self.active_catalog(), table, alias)
                .or_else(|| {
                    // v7.12.3 — GIN-accelerated `WHERE col @@
                    // tsquery` when the column has a `USING gin`
                    // index. Returns an over-approximate candidate
                    // set; the WHERE re-eval loop below verifies
                    // the full `@@` predicate per row.
                    try_gin_seek(w, schema_cols, self.active_catalog(), table, alias, &ctx)
                })
                .or_else(|| {
                    // v7.15.0 — trigram-GIN-accelerated
                    // `WHERE col LIKE / ILIKE '<pat>'` when the
                    // column has a `gin_trgm_ops` GIN index.
                    // Over-approximate candidate set; the WHERE
                    // re-eval verifies the LIKE per row.
                    try_trgm_seek(w, schema_cols, table, alias)
                })
        });

        // Aggregate path: filter rows first, then hand off to the
        // aggregate executor which does its own projection + ORDER BY.
        if aggregate::uses_aggregate(stmt) {
            let mut filtered: Vec<&Row> = Vec::new();
            // v6.2.6 — Memoize: per-query LRU cache for correlated
            // scalar subqueries. Fresh per row-loop entry so each
            // SELECT execution gets an isolated cache.
            let mut memo = memoize::MemoizeCache::new();
            if let Some(rows) = &indexed_rows {
                for cow in rows {
                    let row = cow.as_ref();
                    if let Some(where_expr) = &stmt.where_ {
                        let cond = self.eval_expr_with_correlated(
                            where_expr,
                            row,
                            &ctx,
                            cancel,
                            Some(&mut memo),
                        )?;
                        if !matches!(cond, Value::Bool(true)) {
                            continue;
                        }
                    }
                    filtered.push(row);
                }
            } else {
                for i in 0..table.row_count() {
                    let row = &table.rows()[i];
                    if let Some(where_expr) = &stmt.where_ {
                        let cond = self.eval_expr_with_correlated(
                            where_expr,
                            row,
                            &ctx,
                            cancel,
                            Some(&mut memo),
                        )?;
                        if !matches!(cond, Value::Bool(true)) {
                            continue;
                        }
                    }
                    filtered.push(row);
                }
            }
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let mut agg = aggregate::run(
                stmt,
                &filtered,
                schema_cols,
                Some(alias),
                Some(&agg_correlated),
            )?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset_literal(), stmt.limit_literal());
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, schema_cols, alias)?;
        // v7.19 P5 — single-table SELECT path for SRF
        // `SELECT unnest(arr) FROM t` shape. Detect a top-level
        // unnest in the projection list. When present, the
        // per-row processor emits one output row per array
        // element (broadcasting non-SRF projections from the
        // same input row). Empty / NULL arrays emit zero rows
        // for that input — PG semantics.
        let srf_position = projection.iter().position(|p| is_top_level_unnest(&p.expr));

        // Materialise the filter pass into `(order_key, projected_row)`
        // tuples. The order key is `None` when there's no ORDER BY clause.
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::new();
        // v6.2.6 — Memoize per-row WHERE eval shares one cache.
        let mut memo = memoize::MemoizeCache::new();
        // Inline the per-row work in a closure so the indexed and full-
        // scan branches share the body.
        let mut process_row = |row: &Row, loop_idx: usize| -> Result<(), EngineError> {
            if loop_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(where_expr) = &stmt.where_ {
                let cond =
                    self.eval_expr_with_correlated(where_expr, row, &ctx, cancel, Some(&mut memo))?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                build_order_keys(&stmt.order_by, row, &ctx)?
            };
            if let Some(srf_idx) = srf_position {
                let srf_arg = top_level_unnest_arg(&projection[srf_idx].expr)
                    .expect("checked by is_top_level_unnest above");
                let arr_val = eval::eval_expr(srf_arg, row, &ctx)?;
                let elements = array_value_to_elements(&arr_val)?;
                for elem in elements {
                    let mut values = Vec::with_capacity(projection.len());
                    for (i, p) in projection.iter().enumerate() {
                        if i == srf_idx {
                            values.push(elem.clone());
                        } else {
                            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
                        }
                    }
                    tagged.push((order_keys.clone(), Row::new(values)));
                }
            } else {
                let mut values = Vec::with_capacity(projection.len());
                for p in &projection {
                    // v7.24 (round-16 B) — correlated-aware.
                    values.push(self.eval_expr_with_correlated(&p.expr, row, &ctx, cancel, None)?);
                }
                tagged.push((order_keys, Row::new(values)));
            }
            Ok(())
        };
        if let Some(rows) = &indexed_rows {
            for (loop_idx, cow) in rows.iter().enumerate() {
                process_row(cow.as_ref(), loop_idx)?;
            }
        } else {
            for i in 0..table.row_count() {
                process_row(&table.rows()[i], i)?;
            }
        }

        if !stmt.order_by.is_empty() {
            // Partial-sort fast path: when LIMIT is small relative to
            // the row count, select_nth_unstable + sort just the
            // prefix is O(n + k log k) instead of O(n log n). DISTINCT
            // requires the full sort because de-dup happens after.
            // WITH TIES likewise needs the full sort so the tie
            // extension can scan past `limit` to find rows that
            // share the last-kept row's key.
            let keep = if stmt.distinct || stmt.limit_with_ties {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }

        // v7.17.0 Phase 3.P0-49 — `FETCH FIRST … WITH TIES` extends
        // past the truncated tail through every row that shares the
        // last-kept row's ORDER BY key. The tie check uses the
        // already-computed `(order_keys, row)` pairs so it matches
        // the sort comparator exactly. DISTINCT + WITH TIES falls
        // through to the no-ties path (PG also disallows their
        // combination; SPG silently drops the tie extension here so
        // the customer doesn't see a hard error mid-query — the
        // user-visible result is still correct, just narrower).
        let output_rows: Vec<Row> = if stmt.limit_with_ties && !stmt.distinct {
            apply_offset_and_limit_tagged(
                &mut tagged,
                stmt.offset_literal(),
                stmt.limit_literal(),
                true,
            );
            tagged.into_iter().map(|(_, r)| r).collect()
        } else {
            let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
            if stmt.distinct {
                output_rows = dedup_rows(output_rows);
            }
            apply_offset_and_limit(
                &mut output_rows,
                stmt.offset_literal(),
                stmt.limit_literal(),
            );
            output_rows
        };

        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();

        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }

    /// Multi-table SELECT executor (one or more JOIN peers).
    ///
    /// v1.10 builds the joined row set up-front via nested-loop joins,
    /// then runs WHERE + projection + ORDER BY against the combined
    /// rows. No index seek. Aggregates and DISTINCT still work because
    /// the executor delegates projection through the same shared paths.
    #[allow(clippy::too_many_lines)]
    /// v7.13.2 — mailrs round-6 S5. Resolve a TableRef into an
    /// owned (rows, schema) pair. Catalog tables clone their hot
    /// rows + schema; UNNEST table refs evaluate their array
    /// expression once and synthesise a single-column row set
    /// using the same dispatch as `exec_select_unnest`. Used by
    /// the joined-select path so UNNEST can appear in any FROM
    /// position, not just as the primary.
    fn materialise_table_ref(
        &self,
        tref: &TableRef,
    ) -> Result<(Vec<Row>, Vec<ColumnSchema>), EngineError> {
        if let Some(expr) = tref.unnest_expr.as_deref() {
            let empty_schema: Vec<ColumnSchema> = Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let dummy_row = Row::new(Vec::new());
            let (elem_dtype, rows) =
                match eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)? {
                    Value::Null => (DataType::Text, Vec::new()),
                    Value::TextArray(items) => (
                        DataType::Text,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(s) => Value::Text(s),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    Value::IntArray(items) => (
                        DataType::Int,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::Int(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    Value::BigIntArray(items) => (
                        DataType::BigInt,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::BigInt(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "unnest() expects an array argument, got {:?}",
                            other.data_type()
                        )));
                    }
                };
            let alias = tref.alias.clone().unwrap_or_else(|| "unnest".to_string());
            let col_name = tref.unnest_column_aliases.first().cloned().unwrap_or(alias);
            return Ok((
                rows,
                alloc::vec![ColumnSchema::new(col_name, elem_dtype, true)],
            ));
        }
        let table =
            self.active_catalog()
                .get(&tref.name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: tref.name.clone(),
                })?;
        let rows: Vec<Row> = table.rows().iter().cloned().collect();
        let cols = table.schema().columns.clone();
        Ok((rows, cols))
    }

    /// v7.28 (round-22) — materialise a plain table ref with
    /// single-table predicates pushed BELOW the clone: an indexed
    /// `col = literal` narrows to the matching row ids before any
    /// row is cloned, the rest filter linearly. A correlated
    /// subquery body like `… JOIN messages m2 ON …
    /// WHERE m2.thread_id = '<outer>'` runs per GROUP — without
    /// this it cloned + scanned the full 24k-row table 23.5k times.
    /// Falls back to the plain path for non-table refs.
    fn materialise_table_ref_filtered(
        &self,
        tref: &TableRef,
        preds: &[&Expr],
    ) -> Result<(Vec<Row>, Vec<ColumnSchema>), EngineError> {
        if preds.is_empty()
            || tref.unnest_expr.is_some()
            || tref.lateral_subquery.is_some()
            || tref.as_of_segment.is_some()
        {
            return self.materialise_table_ref(tref);
        }
        let Some(table) = self.active_catalog().get(&tref.name) else {
            return self.materialise_table_ref(tref);
        };
        let cols = table.schema().columns.clone();
        let alias = tref.alias.as_deref().unwrap_or(tref.name.as_str());
        // Index seek on the first `col = literal` predicate with a
        // BTree on that column.
        let mut seeded: Option<Vec<usize>> = None;
        for p in preds {
            if let Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } = p
            {
                let pair = match (lhs.as_ref(), rhs.as_ref()) {
                    (Expr::Column(c), Expr::Literal(l)) | (Expr::Literal(l), Expr::Column(c)) => {
                        Some((c, l))
                    }
                    _ => None,
                };
                if let Some((c, l)) = pair
                    && c.qualifier
                        .as_deref()
                        .is_none_or(|q| q.eq_ignore_ascii_case(alias))
                    && let Some(pos) = cols.iter().position(|s| s.name == c.name)
                    && let Some(idx) = table.index_on(pos)
                    && let Some(key) = spg_storage::IndexKey::from_value(&eval::literal_to_value(l))
                {
                    let mut ids = Vec::new();
                    let mut all_hot = true;
                    for loc in idx.lookup_eq(&key) {
                        match *loc {
                            spg_storage::RowLocator::Hot(i) => ids.push(i),
                            spg_storage::RowLocator::Cold { .. } => {
                                all_hot = false;
                                break;
                            }
                        }
                    }
                    if all_hot {
                        seeded = Some(ids);
                        break;
                    }
                }
            }
        }
        let ctx = EvalContext::new(&cols, Some(alias));
        let mut out: Vec<Row> = Vec::new();
        let push_if = |row: &Row, out: &mut Vec<Row>| -> Result<(), EngineError> {
            for p in preds {
                let v = eval::eval_expr(p, row, &ctx).map_err(EngineError::Eval)?;
                if !matches!(v, Value::Bool(true)) {
                    return Ok(());
                }
            }
            out.push(row.clone());
            Ok(())
        };
        match seeded {
            Some(ids) => {
                for i in ids {
                    if let Some(row) = table.rows().get(i) {
                        push_if(row, &mut out)?;
                    }
                }
            }
            None => {
                for row in table.rows().iter() {
                    push_if(row, &mut out)?;
                }
            }
        }
        Ok((out, cols))
    }

    /// v7.17.0 Phase 3.P0-43 — materialise a `FROM` with one or more
    /// JOINs into `(combined_schema, filtered_rows)`. The combined
    /// schema uses composite `alias.col` column names so the
    /// qualifier-aware column resolver finds every join peer by
    /// exact match; the filtered rows are the join cross-product
    /// after the optional WHERE clause is applied.
    ///
    /// Shared by `exec_joined_select` and the JOIN branch of
    /// `exec_select_with_window`; both paths used to inline the
    /// same nested-loop logic and the window path rejected JOIN
    /// outright.
    /// v7.28 (round-22) — resolve a Column reference against a
    /// composite ("alias.col") schema slice. Bare names match a
    /// unique ".col" suffix.
    fn composite_col_pos(schema: &[ColumnSchema], c: &spg_sql::ast::ColumnName) -> Option<usize> {
        if let Some(q) = &c.qualifier {
            let composite = alloc::format!("{q}.{}", c.name);
            return schema.iter().position(|s| s.name == composite);
        }
        let suffix = alloc::format!(".{}", c.name);
        let mut hits = schema
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name.ends_with(&suffix) || s.name == c.name);
        let first = hits.next();
        if hits.next().is_some() {
            return None; // ambiguous — leave to the residual evaluator
        }
        first.map(|(i, _)| i)
    }

    /// v7.28 (round-22) — resolve a Column against ONE peer's own
    /// columns (right side of a join): `alias.col` or a bare name.
    fn peer_col_pos(
        peer_alias: &str,
        peer_cols: &[ColumnSchema],
        c: &spg_sql::ast::ColumnName,
    ) -> Option<usize> {
        if let Some(q) = &c.qualifier
            && !q.eq_ignore_ascii_case(peer_alias)
        {
            return None;
        }
        peer_cols.iter().position(|s| s.name == c.name)
    }

    /// v7.28 (round-22) — drop the VALUES of columns the statement
    /// never references (schema and positions stay; the value
    /// becomes NULL, so a 30 KB body column costs nothing through
    /// the join pipeline instead of being cloned per row).
    fn null_out_unreferenced(
        rows: &mut [Row],
        cols: &[ColumnSchema],
        alias: &str,
        needed: &alloc::collections::BTreeSet<(String, String)>,
    ) {
        let keep: Vec<bool> = cols
            .iter()
            .map(|c| needed.contains(&(alias.to_string(), c.name.clone())))
            .collect();
        if keep.iter().all(|k| *k) {
            return;
        }
        for row in rows.iter_mut() {
            for (i, k) in keep.iter().enumerate() {
                if !*k && i < row.values.len() {
                    row.values[i] = Value::Null;
                }
            }
        }
    }

    fn build_joined_filtered_rows(
        &self,
        from: &FromClause,
        where_: Option<&Expr>,
        cancel: CancelToken<'_>,
        needed: Option<&alloc::collections::BTreeSet<(String, String)>>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row>), EngineError> {
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        // v7.28 (round-22) — single-table predicate pushdown. WHERE
        // conjuncts whose every column is QUALIFIED with one table's
        // alias filter that table BEFORE the join (with an index
        // seek when one matches `col = literal`). Only the primary
        // and INNER peers are eligible — pre-filtering a LEFT peer
        // would change which rows NULL-extend. Pushed conjuncts stay
        // in WHERE too (idempotent), so correctness never depends on
        // the pushdown.
        let mut primary_preds: Vec<&Expr> = Vec::new();
        let mut peer_preds: Vec<Vec<&Expr>> = alloc::vec![Vec::new(); from.joins.len()];
        if let Some(w) = where_ {
            for sub in reorder::split_and_conjunctions(w) {
                if expr_has_subquery(sub) || aggregate::contains_aggregate(sub) {
                    continue;
                }
                let mut quals: Vec<&str> = Vec::new();
                let mut all_qualified = true;
                collect_column_qualifiers(sub, &mut quals, &mut all_qualified);
                if !all_qualified || quals.is_empty() {
                    continue;
                }
                let q0 = quals[0];
                if !quals.iter().all(|q| q.eq_ignore_ascii_case(q0)) {
                    continue;
                }
                if q0.eq_ignore_ascii_case(&primary_alias) {
                    primary_preds.push(sub);
                    continue;
                }
                for (i, j) in from.joins.iter().enumerate() {
                    if matches!(j.kind, JoinKind::Inner)
                        && j.table.lateral_subquery.is_none()
                        && q0.eq_ignore_ascii_case(
                            j.table.alias.as_deref().unwrap_or(j.table.name.as_str()),
                        )
                    {
                        peer_preds[i].push(sub);
                        break;
                    }
                }
            }
        }
        // v7.28 (round-22) — table-order swap: when the primary has
        // no pushed predicate but an INNER peer does, start from the
        // filtered peer instead. Equi-joins commute; output columns
        // resolve by composite name, so downstream projection is
        // order-independent. (A correlated subquery body like
        // `FROM email_analysis e2 JOIN messages m2 … WHERE
        // m2.thread_id = '<outer>'` otherwise clones the whole
        // unfiltered primary once per outer group.)
        let mut from_owned;
        let mut from = from;
        // Safety: swapping reorders which table joins FIRST, so it is
        // only legal when the FIRST join's ON references no table
        // beyond {primary, first peer} (a later peer's ON may name
        // the original primary, which must already be in the
        // combined row when that peer joins). Restrict to i == 0 AND
        // an ON whose qualifiers all live in those two tables.
        if primary_preds.is_empty()
            && let Some(j0) = from.joins.first()
            && matches!(j0.kind, JoinKind::Inner)
            && j0.table.lateral_subquery.is_none()
            && !peer_preds[0].is_empty()
        {
            let peer_alias = j0.table.alias.as_deref().unwrap_or(j0.table.name.as_str());
            let on_safe = j0.on.as_ref().is_some_and(|on| {
                let mut quals: Vec<&str> = Vec::new();
                let mut all_q = true;
                collect_column_qualifiers(on, &mut quals, &mut all_q);
                all_q
                    && quals.iter().all(|q| {
                        q.eq_ignore_ascii_case(&primary_alias) || q.eq_ignore_ascii_case(peer_alias)
                    })
            });
            if on_safe {
                from_owned = from.clone();
                core::mem::swap(&mut from_owned.primary, &mut from_owned.joins[0].table);
                primary_preds = peer_preds[0].drain(..).collect();
                from = &from_owned;
            }
        }
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        let (mut primary_rows, primary_cols) =
            self.materialise_table_ref_filtered(&from.primary, &primary_preds)?;
        if let Some(needed) = needed {
            Self::null_out_unreferenced(&mut primary_rows, &primary_cols, &primary_alias, needed);
        }
        // v7.17.0 Phase 3.P0-41 — LATERAL peers can't be
        // pre-materialised because their rows depend on outer
        // columns. For each peer, build either an eager
        // (rows, schema) pair or a "lateral" sentinel carrying
        // just the schema and the inner SELECT to re-run per
        // outer row.
        #[allow(clippy::type_complexity)]
        let mut joined: Vec<JoinedPeer<'_>> = Vec::new();
        for j in &from.joins {
            let a = j
                .table
                .alias
                .as_deref()
                .unwrap_or(j.table.name.as_str())
                .to_string();
            if let Some(inner_box) = &j.table.lateral_subquery {
                // Probe schema by running the inner SELECT against a
                // NULL-padded outer context. The probe gives us the
                // projection's column shape; rows materialise per
                // left-row below.
                let schema = self.lateral_probe_schema(inner_box)?;
                joined.push(JoinedPeer {
                    eager_rows: None,
                    cols: schema,
                    alias: a,
                    kind: j.kind,
                    on: j.on.as_ref(),
                    lateral: Some(inner_box.as_ref()),
                    join_table: None,
                });
            } else {
                let pidx = from
                    .joins
                    .iter()
                    .position(|jj| core::ptr::eq(jj, j))
                    .unwrap_or(0);
                // v7.28 - defer materialisation for plain tables with
                // no pushed predicate: the index-nested-loop path may
                // avoid cloning the table entirely.
                let plain = j.table.unnest_expr.is_none() && j.table.as_of_segment.is_none();
                if plain
                    && peer_preds[pidx].is_empty()
                    && let Some(t) = self.active_catalog().get(&j.table.name)
                {
                    joined.push(JoinedPeer {
                        eager_rows: None,
                        cols: t.schema().columns.clone(),
                        alias: a,
                        kind: j.kind,
                        on: j.on.as_ref(),
                        lateral: None,
                        join_table: Some(j.table.name.clone()),
                    });
                    continue;
                }
                let (mut rows, cols) =
                    self.materialise_table_ref_filtered(&j.table, &peer_preds[pidx])?;
                if let Some(needed) = needed {
                    Self::null_out_unreferenced(&mut rows, &cols, &a, needed);
                }
                joined.push(JoinedPeer {
                    eager_rows: Some(rows),
                    cols,
                    alias: a,
                    kind: j.kind,
                    on: j.on.as_ref(),
                    lateral: None,
                    join_table: Some(j.table.name.clone()),
                });
            }
        }
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &primary_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{primary_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for peer in &joined {
            for col in &peer.cols {
                combined_schema.push(ColumnSchema::new(
                    alloc::format!("{}.{}", peer.alias, col.name),
                    col.ty,
                    col.nullable,
                ));
            }
        }
        let ctx = EvalContext::new(&combined_schema, None);
        // v7.28 (round-22) - intermediate-row ceiling: a join whose
        // working set explodes errors instead of eating the host
        // (mailrs watched RSS climb to 7 GiB of 15 before a manual
        // restart). The ceiling is per join STAGE, not per query.
        const MAX_JOIN_INTERMEDIATE_ROWS: usize = 4_000_000;
        let mut working: Vec<Row> = primary_rows;
        // Track the per-row width consumed by the outer left side so
        // each lateral evaluation sees the correct schema slice.
        let mut consumed_cols = primary_cols.len();
        for peer in &joined {
            if working.len() > MAX_JOIN_INTERMEDIATE_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "join intermediate result exceeds {MAX_JOIN_INTERMEDIATE_ROWS} rows ({} so far) - add join predicates",
                    working.len()
                )));
            }
            let right_arity = peer.cols.len();
            let mut next: Vec<Row> = Vec::new();
            // v7.28 (round-22) — hash equi-join. The old path CLONED
            // the full combined row for EVERY (left, right) pair and
            // then evaluated ON — O(L×R) row materialisations (a
            // 24k × 6k LEFT JOIN = 1.5e8 multi-KB clones; the inbox
            // query never returned). Extract `left_col = right_col`
            // conjuncts from ON, build a hash on the (smaller,
            // already-materialised) right side, and only materialise
            // matching pairs. Residual ON conjuncts evaluate on the
            // candidates. NULL keys never match (SQL equality).
            let mut eq_pairs: Vec<(usize, usize)> = Vec::new(); // (left combined pos, right peer pos)
            let mut residual: Vec<&Expr> = Vec::new();
            if let (Some(on_expr), None) = (peer.on, peer.lateral) {
                for sub in reorder::split_and_conjunctions(on_expr) {
                    let mut matched = None;
                    if let Expr::Binary {
                        lhs,
                        op: spg_sql::ast::BinOp::Eq,
                        rhs,
                    } = sub
                        && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
                    {
                        let left_slice = &combined_schema[..consumed_cols];
                        if let (Some(l), Some(r)) = (
                            Self::composite_col_pos(left_slice, a),
                            Self::peer_col_pos(&peer.alias, &peer.cols, b),
                        ) {
                            matched = Some((l, r));
                        } else if let (Some(l), Some(r)) = (
                            Self::composite_col_pos(left_slice, b),
                            Self::peer_col_pos(&peer.alias, &peer.cols, a),
                        ) {
                            matched = Some((l, r));
                        }
                    }
                    match matched {
                        Some(pair) => eq_pairs.push(pair),
                        None => residual.push(sub),
                    }
                }
            }
            // v7.28 (round-22) - index-nested-loop: when the working
            // set is small and the peer's join column has a BTree,
            // seek per left row instead of materialising the whole
            // peer table (a correlated subquery body otherwise
            // clones the full table once per outer group).
            const INL_MAX_LEFT: usize = 1024;
            if let Some(tname) = &peer.join_table
                && peer.eager_rows.is_none()
                && !eq_pairs.is_empty()
                && working.len() <= INL_MAX_LEFT
                && let Some(table) = self.active_catalog().get(tname)
                && let Some(idx) = peer
                    .cols
                    .iter()
                    .position(|c| c.name == peer.cols[eq_pairs[0].1].name)
                    .and_then(|pos| table.index_on(pos))
            {
                let (lpos0, _) = eq_pairs[0];
                for left in &working {
                    cancel.check()?;
                    let mut left_matched = false;
                    let key_v = left.values.get(lpos0).cloned().unwrap_or(Value::Null);
                    if !matches!(key_v, Value::Null)
                        && let Some(key) = spg_storage::IndexKey::from_value(&key_v)
                    {
                        for loc in idx.lookup_eq(&key) {
                            let right = match *loc {
                                spg_storage::RowLocator::Hot(i) => match table.rows().get(i) {
                                    Some(r) => r,
                                    None => continue,
                                },
                                spg_storage::RowLocator::Cold { .. } => continue,
                            };
                            // Remaining eq pairs + residual ON check on
                            // the candidate only.
                            let mut ok = true;
                            for (lp, rp) in eq_pairs.iter().skip(1) {
                                let lv = left.values.get(*lp);
                                let rv = right.values.get(*rp);
                                let eq = match (lv, rv) {
                                    (Some(a), Some(b)) => {
                                        !matches!(a, Value::Null)
                                            && !matches!(b, Value::Null)
                                            && value_cmp(a, b) == core::cmp::Ordering::Equal
                                    }
                                    _ => false,
                                };
                                if !eq {
                                    ok = false;
                                    break;
                                }
                            }
                            if !ok {
                                continue;
                            }
                            let mut combined_vals = left.values.clone();
                            combined_vals.extend(right.values.iter().cloned());
                            let combined = Row::new(combined_vals);
                            let keep = if residual.is_empty() {
                                true
                            } else {
                                let mut k = true;
                                for r in &residual {
                                    let cond = self.eval_expr_with_correlated(
                                        r, &combined, &ctx, cancel, None,
                                    )?;
                                    if !matches!(cond, Value::Bool(true)) {
                                        k = false;
                                        break;
                                    }
                                }
                                k
                            };
                            if keep {
                                next.push(combined);
                                left_matched = true;
                            }
                        }
                    }
                    if !left_matched && matches!(peer.kind, JoinKind::Left) {
                        let mut combined_vals = left.values.clone();
                        for _ in 0..right_arity {
                            combined_vals.push(Value::Null);
                        }
                        next.push(Row::new(combined_vals));
                    }
                }
                working = next;
                consumed_cols += right_arity;
                continue;
            }
            // Deferred peer that didn't take the INL path: materialise
            // now (no pushed predicate, full table).
            let lazy_rows: Option<Vec<Row>> = if peer.eager_rows.is_none() && peer.lateral.is_none()
            {
                let tname = peer.join_table.as_deref().unwrap_or("");
                let mut rows: Vec<Row> = self
                    .active_catalog()
                    .get(tname)
                    .map(|t| t.rows().iter().cloned().collect())
                    .unwrap_or_default();
                if let Some(needed) = needed {
                    Self::null_out_unreferenced(&mut rows, &peer.cols, &peer.alias, needed);
                }
                Some(rows)
            } else {
                None
            };
            let eager_view: Option<&Vec<Row>> = peer.eager_rows.as_ref().or(lazy_rows.as_ref());
            if !eq_pairs.is_empty() && peer.lateral.is_none() {
                let rights = eager_view.expect("non-lateral peer eager");
                // v7.29 - hashbrown over BTreeMap: the ordered map
                // paid O(log n) string comparisons per insert/probe
                // (24k-row build sides spent ~100 ms in it).
                let mut table: hashbrown::HashMap<String, Vec<usize>> =
                    hashbrown::HashMap::with_capacity(rights.len());
                let mut keybuf: Vec<Value> = Vec::with_capacity(eq_pairs.len());
                'build: for (ri, right) in rights.iter().enumerate() {
                    keybuf.clear();
                    for (_, rpos) in &eq_pairs {
                        let v = right.values.get(*rpos).cloned().unwrap_or(Value::Null);
                        if matches!(v, Value::Null) {
                            continue 'build;
                        }
                        keybuf.push(v);
                    }
                    table
                        .entry(aggregate::encode_key(&keybuf))
                        .or_default()
                        .push(ri);
                }
                for left in &working {
                    cancel.check()?;
                    let mut left_matched = false;
                    keybuf.clear();
                    let mut left_has_null = false;
                    for (lpos, _) in &eq_pairs {
                        let v = left.values.get(*lpos).cloned().unwrap_or(Value::Null);
                        if matches!(v, Value::Null) {
                            left_has_null = true;
                            break;
                        }
                        keybuf.push(v);
                    }
                    if !left_has_null
                        && let Some(cands) = table.get(&aggregate::encode_key(&keybuf))
                    {
                        for &ri in cands {
                            let right = &rights[ri];
                            let mut combined_vals = left.values.clone();
                            combined_vals.extend(right.values.iter().cloned());
                            let combined = Row::new(combined_vals);
                            let keep = if residual.is_empty() {
                                true
                            } else {
                                let mut ok = true;
                                for r in &residual {
                                    let cond = self.eval_expr_with_correlated(
                                        r, &combined, &ctx, cancel, None,
                                    )?;
                                    if !matches!(cond, Value::Bool(true)) {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            };
                            if keep {
                                next.push(combined);
                                left_matched = true;
                            }
                        }
                    }
                    if !left_matched && matches!(peer.kind, JoinKind::Left) {
                        let mut combined_vals = left.values.clone();
                        for _ in 0..right_arity {
                            combined_vals.push(Value::Null);
                        }
                        next.push(Row::new(combined_vals));
                    }
                }
                working = next;
                consumed_cols += right_arity;
                debug_assert!(consumed_cols <= combined_schema.len());
                continue;
            }
            // Fallback: nested loop (lateral peers, non-equi ON).
            for left in &working {
                cancel.check()?;
                let mut left_matched = false;
                let per_left_rrows: alloc::borrow::Cow<'_, [Row]> = match peer.lateral {
                    Some(inner) => {
                        // Substitute outer columns and run the inner
                        // SELECT against the current left row's slice
                        // of the combined schema.
                        let outer_schema = &combined_schema[..consumed_cols];
                        let rows = self.materialise_lateral_for_outer(inner, outer_schema, left)?;
                        alloc::borrow::Cow::Owned(rows)
                    }
                    None => {
                        let r = eager_view.expect("non-lateral peer eager");
                        alloc::borrow::Cow::Borrowed(r.as_slice())
                    }
                };
                for right in per_left_rrows.as_ref() {
                    let mut combined_vals = left.values.clone();
                    combined_vals.extend(right.values.iter().cloned());
                    let combined = Row::new(combined_vals);
                    let keep = if let Some(on_expr) = peer.on {
                        // v7.24.1 — correlated-aware (subqueries in
                        // ON referencing earlier join columns).
                        let cond =
                            self.eval_expr_with_correlated(on_expr, &combined, &ctx, cancel, None)?;
                        matches!(cond, Value::Bool(true))
                    } else {
                        true
                    };
                    if keep {
                        next.push(combined);
                        left_matched = true;
                    }
                }
                if !left_matched && matches!(peer.kind, JoinKind::Left) {
                    let mut combined_vals = left.values.clone();
                    for _ in 0..right_arity {
                        combined_vals.push(Value::Null);
                    }
                    next.push(Row::new(combined_vals));
                }
            }
            working = next;
            if working.len() > MAX_JOIN_INTERMEDIATE_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "join intermediate result exceeds {MAX_JOIN_INTERMEDIATE_ROWS} rows ({} so far) - add join predicates",
                    working.len()
                )));
            }
            consumed_cols += right_arity;
            debug_assert!(consumed_cols <= combined_schema.len());
        }
        let mut filtered: Vec<Row> = Vec::new();
        // v7.24 (round-16 B) — the joined WHERE filter ran the plain
        // row evaluator, so a correlated EXISTS/IN/scalar subquery
        // under a JOIN hit "subquery reached row eval". Route through
        // the correlated-aware evaluator (memoized, same as the
        // single-table path).
        let mut memo = memoize::MemoizeCache::default();
        for row in working {
            if let Some(where_expr) = where_ {
                let cond = self.eval_expr_with_correlated(
                    where_expr,
                    &row,
                    &ctx,
                    cancel,
                    Some(&mut memo),
                )?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            filtered.push(row);
        }
        Ok((combined_schema, filtered))
    }

    /// v7.17.0 Phase 3.P0-41 — probe a LATERAL subquery's projection
    /// schema by running it once with a NULL-padded outer context.
    /// The probe never materialises real outer rows; it just executes
    /// the inner SELECT with `outer_alias.col` references substituted
    /// to NULL so the projection's type inference is exercised.
    fn lateral_probe_schema(
        &self,
        inner: &SelectStatement,
    ) -> Result<Vec<ColumnSchema>, EngineError> {
        // Substitute every qualified column reference whose qualifier
        // does NOT match an in-subquery FROM alias with NULL. The
        // safest probe is to walk the inner SELECT and replace any
        // `<qual>.<col>` whose qual isn't bound inside the subquery
        // with a Null literal. For the v7.17 probe we just run the
        // unmodified subquery and surface the columns; if it fails
        // (e.g. references an outer column the probe can't resolve),
        // we synthesise a best-effort schema from the SELECT items
        // by inferring a single Text-typed column per projection.
        match self.execute_readonly_select_for_lateral_probe(inner) {
            Ok(QueryResult::Rows { columns, .. }) => Ok(columns),
            // Best-effort fallback: each SELECT item becomes a TEXT
            // column. Real schemas only differ when the inner SELECT
            // references outer columns at projection-time; those
            // queries surface via the substitution path during
            // per-row execution and still return the right values.
            _ => {
                let mut out: Vec<ColumnSchema> = Vec::new();
                for (i, item) in inner.items.iter().enumerate() {
                    let name = match item {
                        SelectItem::Expr { alias: Some(a), .. } => a.clone(),
                        SelectItem::Expr { expr, .. } => synth_lateral_col_name(expr, i),
                        SelectItem::Wildcard => alloc::format!("col{i}"),
                    };
                    out.push(ColumnSchema::new(name, DataType::Text, true));
                }
                Ok(out)
            }
        }
    }

    /// v7.17.0 Phase 3.P0-41 — try the inner LATERAL subquery against
    /// the engine in read-only mode for schema-probe purposes. Failure
    /// is expected when the subquery references an outer column the
    /// probe can't resolve; the caller falls back to a best-effort
    /// schema based on the SELECT items.
    fn execute_readonly_select_for_lateral_probe(
        &self,
        inner: &SelectStatement,
    ) -> Result<QueryResult, EngineError> {
        self.exec_bare_select_cancel(inner, CancelToken::none())
    }

    /// v7.17.0 Phase 3.P0-41 — materialise a LATERAL subquery's rows
    /// for one outer-row context. Walks the inner SELECT, replaces
    /// every `<outer_alias>.<col>` reference whose alias appears in
    /// the outer schema with the literal value from the outer row,
    /// then runs the rewritten SELECT against the engine.
    fn materialise_lateral_for_outer(
        &self,
        inner: &SelectStatement,
        outer_schema: &[ColumnSchema],
        outer_row: &Row,
    ) -> Result<Vec<Row>, EngineError> {
        let mut substituted = inner.clone();
        substitute_outer_columns_multi(&mut substituted, outer_row, outer_schema);
        let result = self.exec_bare_select_cancel(&substituted, CancelToken::none())?;
        match result {
            QueryResult::Rows { rows, .. } => Ok(rows),
            _ => Err(EngineError::Unsupported(
                "LATERAL subquery must be a SELECT (cannot be a write statement)".into(),
            )),
        }
    }

    fn exec_joined_select(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.17.0 Phase 3.P0-43 + P0-41 — delegate the join +
        // WHERE materialisation to the shared helper so the LATERAL
        // / UNNEST / regular-catalog paths route through one place.
        // (`build_joined_filtered_rows` carries LATERAL support as
        // of Phase 3.P0-41.) Downstream we still handle aggregate /
        // projection / ORDER BY / DISTINCT / LIMIT inline because
        // those depend on the SelectStatement's items list.
        let (combined_schema, filtered) = {
            let mut needed = alloc::collections::BTreeSet::new();
            let prunable = collect_qualified_refs(stmt, &mut needed).is_some();
            self.build_joined_filtered_rows(
                from,
                stmt.where_.as_ref(),
                cancel,
                if prunable { Some(&needed) } else { None },
            )?
        };
        let ctx = EvalContext::new(&combined_schema, None);
        // Aggregate path: handle GROUP BY / aggregate calls over the
        // joined+filtered rows.
        if aggregate::uses_aggregate(stmt) {
            let refs: Vec<&Row> = filtered.iter().collect();
            // v7.29 — a per-query memo so correlated scalar
            // subqueries batch-evaluate once (group map) instead of
            // executing per group.
            let agg_memo = core::cell::RefCell::new(memoize::MemoizeCache::default());
            let agg_correlated = |e: &Expr, r: &Row, c: &EvalContext<'_>| {
                self.eval_expr_with_correlated(e, r, c, cancel, Some(&mut agg_memo.borrow_mut()))
                    .map_err(|err| match err {
                        EngineError::Eval(ev) => ev,
                        other => eval::EvalError::TypeMismatch {
                            detail: alloc::format!("{other}"),
                        },
                    })
            };
            let mut agg =
                aggregate::run(stmt, &refs, &combined_schema, None, Some(&agg_correlated))?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset_literal(), stmt.limit_literal());
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, &combined_schema, "")?;
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::new();
        let mut proj_memo = memoize::MemoizeCache::default();
        for row in &filtered {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                // v7.24 (round-16 B) — select-list subqueries under a
                // JOIN go through the correlated-aware evaluator too.
                values.push(self.eval_expr_with_correlated(
                    &p.expr,
                    row,
                    &ctx,
                    cancel,
                    Some(&mut proj_memo),
                )?);
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                build_order_keys(&stmt.order_by, row, &ctx)?
            };
            tagged.push((order_keys, Row::new(values)));
        }
        if !stmt.order_by.is_empty() {
            let keep = if stmt.distinct {
                None
            } else {
                stmt.limit_literal()
                    .map(|l| l as usize + stmt.offset_literal().map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }
        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(
            &mut output_rows,
            stmt.offset_literal(),
            stmt.limit_literal(),
        );
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }
}

/// One row-producing projection: an expression to evaluate, the resulting
/// column's user-visible name, its inferred type, and nullability.
#[derive(Debug, Clone)]
struct ProjectedItem {
    expr: Expr,
    output_name: String,
    ty: DataType,
    nullable: bool,
}

/// Dedupe a row set, preserving first-seen order. `Row`'s `PartialEq` is
/// structural (`Vec<Value>` ⇒ pairwise `Value` equality), which gives SQL
/// `NULL = NULL → TRUE` and `NaN = NaN → FALSE`. The first agrees with
/// the spec's "two NULLs are not distinct"; the second is a tolerated
/// quirk for v1 (no NaN literals are reachable from the SQL surface).
fn dedup_rows(rows: Vec<Row>) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for r in rows {
        if !out.iter().any(|seen| seen == &r) {
            out.push(r);
        }
    }
    out
}

/// Coerce a `Value` to an `f64` sort key for ORDER BY. Numbers map directly;
/// NULL sorts last (treated as `+∞`); booleans are 0.0 / 1.0; text uses lex
/// order via the byte values; vectors are not sortable.
fn value_to_order_key(v: &Value) -> Result<f64, EngineError> {
    match v {
        Value::Null => Ok(f64::INFINITY),
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        Value::Date(d) => Ok(f64::from(*d)),
        #[allow(clippy::cast_precision_loss)]
        Value::Timestamp(t) => Ok(*t as f64),
        // v7.17.0 Phase 3.P0-32 — PG TIME ordered by underlying
        // i64 microseconds (matches wall-clock ordering).
        #[allow(clippy::cast_precision_loss)]
        Value::Time(us) => Ok(*us as f64),
        // v7.17.0 Phase 3.P0-33 — MySQL YEAR ordered by underlying
        // u16 (matches calendar ordering; zero-year sentinel
        // sorts before 1901).
        Value::Year(y) => Ok(f64::from(*y)),
        // v7.17.0 Phase 3.P0-34 — PG TIMETZ ordered by the
        // UTC-equivalent microseconds (local wall - offset). Two
        // values for the same physical instant in different zones
        // sort equal — matches PG TIMETZ index behaviour.
        #[allow(clippy::cast_precision_loss)]
        Value::TimeTz { us, offset_secs } => Ok((us - i64::from(*offset_secs) * 1_000_000) as f64),
        // v7.17.0 Phase 3.P0-35 — PG MONEY ordered by i64 cents.
        #[allow(clippy::cast_precision_loss)]
        Value::Money(c) => Ok(*c as f64),
        // v7.17.0 Phase 3.P0-38 — range ordering is not supported
        // in v7.17.0 (needs lex-then-inclusivity tiebreak).
        Value::Range { .. } => Err(EngineError::Unsupported(
            "ORDER BY of a range value is not supported in v7.17.0".into(),
        )),
        // v7.17.0 Phase 3.P0-39 — hstore is not orderable.
        Value::Hstore(_) => Err(EngineError::Unsupported(
            "ORDER BY of a hstore value is not supported".into(),
        )),
        // v7.17.0 Phase 3.P0-40 — 2D arrays not orderable.
        Value::IntArray2D(_) | Value::BigIntArray2D(_) | Value::TextArray2D(_) => Err(
            EngineError::Unsupported("ORDER BY of a 2D array is not supported in v7.17.0".into()),
        ),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            // Scaled integer / 10^scale, computed via f64 for sort
            // ordering only. Precision losses here only matter for
            // ORDER BY tie-breaks well past 15 significant digits.
            // `f64::powi` lives in std; we hand-roll the loop so the
            // no_std engine crate doesn't need it.
            let mut divisor = 1.0_f64;
            for _ in 0..*scale {
                divisor *= 10.0;
            }
            Ok((*scaled as f64) / divisor)
        }
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Text(s) => {
            // Lex order by codepoints — good enough for ORDER BY name.
            // Map first 8 bytes packed into u64 as a coarse key; ties fall to
            // partial_cmp Equal. v1.x can swap in a real string comparator.
            let mut key: u64 = 0;
            for &b in s.as_bytes().iter().take(8) {
                key = (key << 8) | u64::from(b);
            }
            #[allow(clippy::cast_precision_loss)]
            Ok(key as f64)
        }
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            Err(EngineError::Unsupported(
                "ORDER BY of a raw vector column is not meaningful — use `<->`".into(),
            ))
        }
        Value::Interval { .. } => Err(EngineError::Unsupported(
            "ORDER BY of an INTERVAL is not supported in v2.11 \
             (months vs micros has no single canonical ordering)"
                .into(),
        )),
        Value::Json(_) => Err(EngineError::Unsupported(
            "ORDER BY of a JSON value is not supported — cast the document to text first".into(),
        )),
        // v7.5.0 — Value is #[non_exhaustive]; future variants need
        // an explicit ORDER BY mapping. Surface as Unsupported until
        // engine support is added.
        _ => Err(EngineError::Unsupported(
            "ORDER BY of this value type is not supported".into(),
        )),
    }
}

/// Try to plan a WHERE clause as an equality lookup against an existing
/// index. Returns the candidate row indices on success; `None` means the
/// caller should fall back to a full scan.
///
/// v0.8 recognises a single top-level `col = literal` (in either operand
/// order). AND chains and range scans land in later milestones.
/// Look for `ORDER BY col <dist-op> literal LIMIT k` against an
/// NSW-indexed vector column. Recognised distance ops: `<->` (L2),
/// `<#>` (inner product), `<=>` (cosine). When a WHERE clause is
/// present, the planner does an "over-fetch and filter" pass — it
/// asks the graph for `k * over_fetch` candidates, evaluates WHERE
/// against each, and trims back to `k`. Returns the row indices in
/// ascending-distance order when the plan applies.
fn try_nsw_knn(
    stmt: &SelectStatement,
    table: &Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<Vec<usize>> {
    if stmt.distinct {
        return None;
    }
    let limit = usize::try_from(stmt.limit_literal()?).ok()?;
    if limit == 0 {
        return None;
    }
    // v6.4.0 — NSW kNN dispatch needs a single ORDER BY key on the
    // distance metric. Multi-key ORDER BY falls through to the
    // generic sort path.
    if stmt.order_by.len() != 1 {
        return None;
    }
    let order = &stmt.order_by[0];
    // NSW kNN returns rows ascending by distance — DESC inverts the
    // natural order, so the planner can't handle it without a sort
    // pass. Fall back to the generic ORDER BY path.
    if order.desc {
        return None;
    }
    let Expr::Binary { lhs, op, rhs } = &order.expr else {
        return None;
    };
    let metric = match op {
        BinOp::L2Distance => spg_storage::NswMetric::L2,
        BinOp::InnerProduct => spg_storage::NswMetric::InnerProduct,
        BinOp::CosineDistance => spg_storage::NswMetric::Cosine,
        _ => return None,
    };
    // Accept both `col <op> literal` and `literal <op> col`.
    let ((Expr::Column(col), literal) | (literal, Expr::Column(col))) =
        (lhs.as_ref(), rhs.as_ref())
    else {
        return None;
    };
    if let Some(q) = &col.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols.iter().position(|s| s.name == col.name)?;
    let query = literal_to_vector(literal)?;
    let idx = spg_storage::nsw_index_on(table, col_pos)?;
    if let Some(where_expr) = &stmt.where_ {
        // Over-fetch and filter. The factor (10×) is a heuristic that
        // covers typical selectivity for the corpus tests; v2.x will
        // make it configurable.
        let over_fetch = limit.saturating_mul(10).max(NSW_OVER_FETCH_FLOOR);
        let candidates = spg_storage::nsw_query(table, &idx.name, &query, over_fetch, metric);
        let ctx = EvalContext::new(schema_cols, Some(table_alias));
        let mut kept: Vec<usize> = Vec::with_capacity(limit);
        for i in candidates {
            let row = &table.rows()[i];
            let cond = eval::eval_expr(where_expr, row, &ctx).ok()?;
            if matches!(cond, Value::Bool(true)) {
                kept.push(i);
                if kept.len() >= limit {
                    break;
                }
            }
        }
        Some(kept)
    } else {
        Some(spg_storage::nsw_query(
            table, &idx.name, &query, limit, metric,
        ))
    }
}

/// Lower bound on the over-fetch pool when WHERE is present — even
/// for tiny `LIMIT 1` queries we keep enough candidates to absorb a
/// few WHERE rejections.
const NSW_OVER_FETCH_FLOOR: usize = 32;

/// Pull a `Vec<f32>` out of a literal-or-cast expression. Returns
/// `None` for anything we can't fold at plan time.
fn literal_to_vector(e: &Expr) -> Option<Vec<f32>> {
    match e {
        Expr::Literal(Literal::Vector(v)) => Some(v.clone()),
        Expr::Cast { expr, .. } => literal_to_vector(expr),
        _ => None,
    }
}

/// Materialise rows in a planner-supplied order (used by the NSW path)
/// without re-running ORDER BY. The projection + LIMIT slot mirror the
/// equivalent block in `exec_bare_select`.
fn materialise_in_order(
    stmt: &SelectStatement,
    table: &Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    ordered_rows: &[usize],
) -> Result<QueryResult, EngineError> {
    let ctx = EvalContext::new(schema_cols, Some(table_alias));
    let projection = build_projection(&stmt.items, schema_cols, table_alias)?;
    let mut output_rows: Vec<Row> = Vec::with_capacity(ordered_rows.len());
    for &i in ordered_rows {
        let row = &table.rows()[i];
        let mut values = Vec::with_capacity(projection.len());
        for p in &projection {
            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
        }
        output_rows.push(Row::new(values));
    }
    apply_offset_and_limit(
        &mut output_rows,
        stmt.offset_literal(),
        stmt.limit_literal(),
    );
    let columns: Vec<ColumnSchema> = projection
        .into_iter()
        .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
        .collect();
    Ok(QueryResult::Rows {
        columns,
        rows: output_rows,
    })
}

/// v7.20 P4 — hot-row POSITION seek for the mutation paths
/// (UPDATE / DELETE index their planned writes by position in
/// `table.rows()`, so the Cow-row shape `try_index_seek`
/// returns doesn't fit). Same top-level-AND recursion and
/// col=literal resolution; the caller re-applies the full WHERE
/// to every returned row so the index only narrows candidates.
///
/// Returns `None` (→ caller full-scans) when no equality leaf
/// hits an index OR any matching locator lives in the cold tier
/// — the mutation paths operate on hot rows, and the PK
/// promote-then-walk upstream already handles the
/// cold-single-row case.
fn try_index_seek_positions(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
) -> Option<Vec<usize>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(p) = try_index_seek_positions(lhs, schema_cols, table, table_alias) {
            return Some(p);
        }
        return try_index_seek_positions(rhs, schema_cols, table, table_alias);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    let locators = idx.lookup_eq(&key);
    let mut out = Vec::with_capacity(locators.len());
    for loc in locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => out.push(i),
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    Some(out)
}

fn try_index_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
) -> Option<Vec<Cow<'a, Row>>> {
    // v7.11.3 — recurse through top-level `AND` so a PG-style
    // composite predicate like `WHERE id = 1 AND created_at > $1`
    // still hits the index on `id`. The caller re-applies the
    // full WHERE expression to each returned row, so dropping the
    // residual conjuncts here is correct — the index just narrows
    // the candidate set.
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        // Try LHS first (typical convention: leading equality on
        // the indexed column comes first in user-written SQL).
        if let Some(rows) = try_index_seek(lhs, schema_cols, catalog, table, table_alias) {
            return Some(rows);
        }
        return try_index_seek(rhs, schema_cols, catalog, table, table_alias);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    let locators = idx.lookup_eq(&key);
    let table_name = table.schema().name.as_str();
    // v5.1: each locator dispatches to either the hot tier (zero-
    // copy borrow of `table.rows()[i]`) or a cold-tier segment
    // (one page read + dense row decode, ~µs scale). Cold rows are
    // returned as `Cow::Owned` so the caller's `&Row` iteration
    // doesn't see a tier distinction; pre-freezer (no cold
    // segments loaded) every locator is `Hot` and every entry is
    // `Cow::Borrowed` — identical cost to the pre-v5.1 path.
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(locators.len());
    for loc in locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => {
                if let Some(row) = table.rows().get(i) {
                    out.push(Cow::Borrowed(row));
                }
            }
            spg_storage::RowLocator::Cold { segment_id, .. } => {
                if let Some(row) = catalog.resolve_cold_locator(table_name, segment_id, &key) {
                    out.push(Cow::Owned(row));
                }
            }
        }
    }
    Some(out)
}

/// v7.12.3 — GIN-accelerated candidate seek for `WHERE col @@ <ts_query>`.
///
/// Recurses through top-level `AND` like [`try_index_seek`] so a
/// composite predicate `WHERE search_vector @@ q AND id > $1` still
/// hits the GIN index on `search_vector` — the caller re-applies the
/// full WHERE expression to each returned candidate, so dropping the
/// `id > $1` residual here stays semantically correct.
///
/// Returns `None` when:
///   - no leaf is a `col @@ <rhs>` shape on a GIN-indexed column;
///   - the RHS can't be const-evaluated to a `Value::TsQuery`
///     (typically because it references row columns);
///   - the resolved `TsQuery` uses query shapes the MVP doesn't
///     accelerate (`Not`, `Phrase` — those fall through to full scan).
///
/// On `Some(rows)` the caller iterates only `rows` and re-evaluates
/// the full `@@` predicate per row, so an over-approximate candidate
/// set is safe.
fn try_gin_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
    ctx: &eval::EvalContext<'_>,
) -> Option<Vec<Cow<'a, Row>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) = try_gin_seek(lhs, schema_cols, catalog, table, table_alias, ctx) {
            return Some(rows);
        }
        return try_gin_seek(rhs, schema_cols, catalog, table, table_alias, ctx);
    }
    // v7.17.0 Phase 3.P0-44 — MySQL `MATCH(col1, col2) AGAINST (...)`
    // desugars into `(to_tsvector(col1) @@ q) OR (to_tsvector(col2) @@ q)`
    // in the parser. To accelerate the multi-column case, walk OR the same
    // way we walk AND: only emit a candidate set if BOTH sides can seek
    // (otherwise the OR result is unbounded and we must fall through to
    // the full scan). Candidates are union'd; the caller's WHERE re-eval
    // verifies the full predicate per row, so duplicates / supersets stay
    // semantically safe.
    if let Expr::Binary {
        lhs,
        op: BinOp::Or,
        rhs,
    } = where_expr
    {
        let left = try_gin_seek(lhs, schema_cols, catalog, table, table_alias, ctx)?;
        let right = try_gin_seek(rhs, schema_cols, catalog, table, table_alias, ctx)?;
        let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(left.len() + right.len());
        out.extend(left);
        out.extend(right);
        return Some(out);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::TsMatch,
        rhs,
    } = where_expr
    else {
        return None;
    };
    // Either side can be the column; pgvector idiom (`vec @@ q`)
    // hits the first arm, FROM-clause-derived (`plainto_tsquery($1)
    // q ... WHERE search_vector @@ q`) the same. CROSS JOIN derived
    // tables resolve `q` to a Column too.
    let (col_pos, query) = resolve_gin_col_query(lhs, rhs, schema_cols, table_alias, ctx)
        .or_else(|| resolve_gin_col_query(rhs, lhs, schema_cols, table_alias, ctx))?;
    // v7.17.0 Phase 3.P0-44 — MySQL `FULLTEXT KEY` builds a
    // `IndexKind::GinFulltext` posting list (Phase 2.2). It shares
    // the same `gin_lookup_word` shape as the tsvector-typed GIN,
    // so the MATCH-AGAINST `@@` predicate (desugared by the parser
    // into `to_tsvector(col) @@ plainto_tsquery('term')`) routes
    // through the same candidate-set seek.
    let idx = table
        .indices()
        .iter()
        .find(|i| i.column_position == col_pos && (i.is_gin() || i.is_gin_fulltext()))?;
    let candidates = gin_query_candidates(idx, &query)?;
    let _ = catalog; // cold-tier row resolution unused in MVP; see below.
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(candidates.len());
    for loc in candidates {
        match loc {
            spg_storage::RowLocator::Hot(i) => {
                if let Some(row) = table.rows().get(i) {
                    out.push(Cow::Borrowed(row));
                }
            }
            // GIN cold-tier rows in the MVP: skipped, matching the
            // full-scan `@@` path which itself only iterates
            // `table.rows()` (hot tier). When v7.13+ adds cold-tier
            // scan-time materialisation for `@@`, the parallel
            // resolution lands here; until then both paths see the
            // same hot-only candidate set so correctness is preserved.
            spg_storage::RowLocator::Cold { .. } => {}
        }
    }
    Some(out)
}

/// v7.15.0 — trigram-GIN-accelerated candidate seek for
/// `WHERE col LIKE '<pat>'` and `WHERE col ILIKE '<pat>'` when
/// the column has a `gin_trgm_ops` GIN index.
///
/// Walks top-level `AND` so multi-predicate WHEREs (`col LIKE
/// 'foo%' AND id > 1`) still hit the trigram index; the caller
/// re-evaluates the full WHERE per candidate row, so dropping
/// non-LIKE conjuncts here stays semantically correct.
///
/// Returns `None` when:
///   - no leaf is `col LIKE/ILIKE <literal>` on a trigram-GIN-
///     indexed column;
///   - the pattern's literal runs are too short to constrain
///     (pattern decomposes into `< 3`-char runs, e.g. `%ab%`);
///   - the pattern doesn't const-evaluate to a TEXT.
fn try_trgm_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
) -> Option<Vec<Cow<'a, Row>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) = try_trgm_seek(lhs, schema_cols, table, table_alias) {
            return Some(rows);
        }
        return try_trgm_seek(rhs, schema_cols, table, table_alias);
    }
    // LIKE node is what carries the column reference + pattern.
    // ILIKE is the same AST node — PG's LIKE/ILIKE both lower
    // through `Expr::Like { expr, pattern, negated }`. The trigram
    // index posting-list keys are already lower-cased and
    // case-folded, so we only need the pattern's literal runs.
    let Expr::Like { expr, pattern, .. } = where_expr else {
        return None;
    };
    // Column side.
    let Expr::Column(c) = expr.as_ref() else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(&c.name))?;
    // Index must exist on that column AND be a trigram-GIN.
    let idx = table
        .indices()
        .iter()
        .find(|i| i.column_position == col_pos && i.is_gin_trgm())?;
    // Pattern side must be a literal TEXT — anything else (column
    // ref, function call, parameter that hasn't been bound yet)
    // falls through to full scan.
    let Expr::Literal(spg_sql::ast::Literal::String(pat)) = pattern.as_ref() else {
        return None;
    };
    let trigrams = spg_storage::trgm::trigrams_from_like_pattern(pat)?;
    // Intersect every trigram's posting list. Empty intersection
    // → empty candidate set (caller short-circuits its row loop).
    let mut iter = trigrams.iter();
    let first = iter.next()?;
    let mut acc: Vec<spg_storage::RowLocator> = {
        let mut v = idx.gin_trgm_lookup(first).to_vec();
        v.sort_by_key(locator_sort_key);
        v.dedup_by_key(|l| locator_sort_key(l));
        v
    };
    for tri in iter {
        let mut next: Vec<spg_storage::RowLocator> = idx.gin_trgm_lookup(tri).to_vec();
        next.sort_by_key(locator_sort_key);
        next.dedup_by_key(|l| locator_sort_key(l));
        // Sorted-merge intersection.
        let mut merged: Vec<spg_storage::RowLocator> =
            Vec::with_capacity(acc.len().min(next.len()));
        let (mut i, mut j) = (0usize, 0usize);
        while i < acc.len() && j < next.len() {
            let lk = locator_sort_key(&acc[i]);
            let rk = locator_sort_key(&next[j]);
            match lk.cmp(&rk) {
                core::cmp::Ordering::Less => i += 1,
                core::cmp::Ordering::Greater => j += 1,
                core::cmp::Ordering::Equal => {
                    merged.push(acc[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        acc = merged;
        if acc.is_empty() {
            break;
        }
    }
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(acc.len());
    for loc in acc {
        if let spg_storage::RowLocator::Hot(i) = loc
            && let Some(row) = table.rows().get(i)
        {
            out.push(Cow::Borrowed(row));
        }
        // Cold-tier rows: skipped in MVP (same as try_gin_seek).
    }
    Some(out)
}

/// v7.12.3 — extract `(column_position, TsQueryAst)` when one side of
/// the binary is a column reference to a GIN-indexed tsvector column
/// and the other side const-evaluates to a `Value::TsQuery`. Returns
/// `None` if the column reference is for the wrong table alias, or if
/// the RHS expression depends on row data.
fn resolve_gin_col_query(
    col_side: &Expr,
    query_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    ctx: &eval::EvalContext<'_>,
) -> Option<(usize, spg_storage::TsQueryAst)> {
    // v7.17.0 Phase 3.P0-44 — the MATCH AGAINST desugar wraps the
    // column in `to_tsvector('simple', col)`, so we peel that wrapper
    // before the column lookup. Direct `col @@ tsquery` paths (the
    // tsvector-typed v7.12 surface) skip the wrapper entirely.
    let column = match col_side {
        Expr::Column(c) => c,
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("to_tsvector") && !args.is_empty() =>
        {
            // PG `to_tsvector` accepts either `to_tsvector(col)` or
            // `to_tsvector(config, col)`. In both shapes the column
            // we care about is the final argument.
            if let Expr::Column(c) = args.last().unwrap() {
                c
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let c = column;
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    // Const-evaluate the query side with an empty row — fails fast
    // (with a `ColumnNotFound` / similar) if the expression actually
    // depends on row data, which is exactly the bail signal we want.
    let empty_row = Row::new(Vec::new());
    let v = eval::eval_expr(query_side, &empty_row, ctx).ok()?;
    let Value::TsQuery(q) = v else { return None };
    Some((pos, q))
}

/// v7.12.3 — walk a `TsQueryAst` against an [`IndexKind::Gin`] index
/// to produce a candidate row-locator set. Returns `None` for query
/// shapes the MVP doesn't accelerate (`Not` / `Phrase` — both bail to
/// full scan since their semantics need either complementation across
/// the whole row set or positional verification beyond what the
/// posting list carries).
///
/// Candidate sets are over-approximate — the caller re-applies the
/// full `@@` predicate per row, so reporting "row was in some
/// posting list" without verifying positions / weights stays correct.
fn gin_query_candidates(
    idx: &spg_storage::Index,
    query: &spg_storage::TsQueryAst,
) -> Option<Vec<spg_storage::RowLocator>> {
    use spg_storage::TsQueryAst;
    match query {
        TsQueryAst::Term { word, .. } => {
            let mut v: Vec<spg_storage::RowLocator> = idx.gin_lookup_word(word).to_vec();
            v.sort_by_key(locator_sort_key);
            v.dedup_by_key(|l| locator_sort_key(l));
            Some(v)
        }
        TsQueryAst::And(l, r) => {
            let mut left = gin_query_candidates(idx, l)?;
            let mut right = gin_query_candidates(idx, r)?;
            left.sort_by_key(locator_sort_key);
            right.sort_by_key(locator_sort_key);
            // Sorted-merge intersection.
            let mut out: Vec<spg_storage::RowLocator> = Vec::new();
            let (mut i, mut j) = (0usize, 0usize);
            while i < left.len() && j < right.len() {
                let lk = locator_sort_key(&left[i]);
                let rk = locator_sort_key(&right[j]);
                match lk.cmp(&rk) {
                    core::cmp::Ordering::Less => i += 1,
                    core::cmp::Ordering::Greater => j += 1,
                    core::cmp::Ordering::Equal => {
                        out.push(left[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            Some(out)
        }
        TsQueryAst::Or(l, r) => {
            let mut out = gin_query_candidates(idx, l)?;
            out.extend(gin_query_candidates(idx, r)?);
            out.sort_by_key(locator_sort_key);
            out.dedup_by_key(|l| locator_sort_key(l));
            Some(out)
        }
        // Not / Phrase bail to full scan in the MVP. Not needs
        // complementation against the whole row set (not represented
        // in the posting-list view); Phrase needs positional
        // verification beyond what `word → rows` carries.
        TsQueryAst::Not(_) | TsQueryAst::Phrase { .. } => None,
    }
}

/// v7.12.3 — total ordering on `RowLocator` for sort/dedup purposes
/// inside the GIN intersection / union loops. Hot rows order by their
/// row index; Cold rows order after all Hot rows, then by
/// `(segment_id, the cold sub-key)`.
fn locator_sort_key(l: &spg_storage::RowLocator) -> (u8, u64, u64) {
    match *l {
        spg_storage::RowLocator::Hot(i) => (0, i as u64, 0),
        spg_storage::RowLocator::Cold {
            segment_id,
            page_offset,
        } => (1, u64::from(segment_id), u64::from(page_offset)),
    }
}

/// v5.2.3: extract `(column_position, IndexKey)` when `where_expr`
/// is a simple `col = literal` predicate suitable for a `BTree` index
/// seek. Used by `exec_update_cancel` / `exec_delete_cancel` to
/// decide whether a write touches a cold-tier row (which requires
/// promote-on-write / shadow-on-delete) before falling through to
/// the hot-tier row walk.
///
/// Returns `None` for any predicate shape the planner can't push
/// down to an index seek — complex WHERE clauses always take the
/// hot-only path (cold rows are immutable to non-indexed writes
/// until a future scan-fanout sub-version).
fn try_pk_predicate(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, IndexKey)> {
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let key = IndexKey::from_value(&value)?;
    Some((col_pos, key))
}

fn resolve_col_literal_pair(
    col_side: &Expr,
    lit_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, Value)> {
    let Expr::Column(c) = col_side else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    let Expr::Literal(l) = lit_side else {
        return None;
    };
    let v = match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::Text(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        // Vector, array and Interval literals can't be used as B-tree
        // index keys. Tell the planner to fall back to full-scan.
        Literal::Vector(_)
        | Literal::Interval { .. }
        | Literal::TextArray(_)
        | Literal::IntArray(_)
        | Literal::BigIntArray(_) => return None,
    };
    Some((pos, v))
}

/// Find the schema entry that a SELECT-list `Expr::Column` refers to.
/// Mirrors `resolve_column` in `eval.rs`, but returns a proper
/// `EngineError` so the projection-build path keeps `UnknownQualifier`
/// vs `ColumnNotFound` distinct.
fn resolve_projection_column<'a>(
    c: &ColumnName,
    schema_cols: &'a [ColumnSchema],
    table_alias: &str,
) -> Result<&'a ColumnSchema, EngineError> {
    if let Some(q) = &c.qualifier {
        let composite = alloc::format!("{q}.{name}", name = c.name);
        if let Some(s) = schema_cols.iter().find(|s| s.name == composite) {
            return Ok(s);
        }
        // Single-table case: the qualifier may equal the active alias —
        // then look for the bare column name.
        if q == table_alias
            && let Some(s) = schema_cols.iter().find(|s| s.name == c.name)
        {
            return Ok(s);
        }
        // For multi-table schemas the qualifier is unknown only if no
        // column bears the "<q>." prefix. For single-table, the alias
        // mismatch alone is enough.
        let prefix = alloc::format!("{q}.");
        let qualifier_known =
            q == table_alias || schema_cols.iter().any(|s| s.name.starts_with(&prefix));
        if !qualifier_known {
            return Err(EngineError::Eval(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            }));
        }
        return Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        }));
    }
    if let Some(s) = schema_cols.iter().find(|s| s.name == c.name) {
        return Ok(s);
    }
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = schema_cols.iter().filter(|s| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Ok(s),
        (Some(_), Some(_)) => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!("ambiguous column reference: {}", c.name),
        })),
        _ => Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        })),
    }
}

fn build_projection(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Result<Vec<ProjectedItem>, EngineError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for col in schema_cols {
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name: col.name.clone(),
                        ty: col.ty,
                        nullable: col.nullable,
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                // Plain column ref keeps full schema info (real type +
                // nullability). For compound expressions try the
                // describe-side function-return-type table first
                // (e.g. `SELECT now()` → Timestamptz, `SELECT
                // concat(…)` → Text). Falls back to nullable Text
                // for shapes the describe path can't resolve.
                if let Expr::Column(c) = expr {
                    let sch = resolve_projection_column(c, schema_cols, table_alias)?;
                    let output_name = alias.clone().unwrap_or_else(|| c.name.clone());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: sch.ty,
                        nullable: sch.nullable,
                    });
                } else if let Some(shape) = describe::describe_expr(expr, schema_cols) {
                    let output_name = alias.clone().unwrap_or_else(|| expr.to_string());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: shape.ty,
                        nullable: shape.nullable,
                    });
                } else {
                    let output_name = alias.clone().unwrap_or_else(|| expr.to_string());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: DataType::Text,
                        nullable: true,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Promote an integer to a NUMERIC value at the requested scale.
/// Rejects values that, after scaling, would overflow the column's
/// precision budget.
fn numeric_from_integer(
    n: i128,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    let factor = pow10_i128(scale);
    let scaled = n.checked_mul(factor).ok_or_else(|| {
        EngineError::Unsupported(alloc::format!(
            "integer overflow scaling value for column `{col_name}` to scale {scale}"
        ))
    })?;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// Float → NUMERIC. Uses round-half-away-from-zero on `x * 10^scale`,
/// then verifies the result fits the column's precision.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn numeric_from_float(
    x: f64,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    if !x.is_finite() {
        return Err(EngineError::Unsupported(alloc::format!(
            "cannot store non-finite float in NUMERIC column `{col_name}`"
        )));
    }
    let mut factor = 1.0_f64;
    for _ in 0..scale {
        factor *= 10.0;
    }
    // Round half-away-from-zero by biasing then casting (`as i128`
    // truncates toward zero, so the bias + truncation gives the
    // desired rounding). `f64::floor` / `ceil` live in std; we don't
    // need them — the cast handles the truncation step.
    let shifted = x * factor;
    let biased = if shifted >= 0.0 {
        shifted + 0.5
    } else {
        shifted - 0.5
    };
    // Range-check before casting back to i128 — the cast itself is
    // saturating in Rust, which would silently truncate huge inputs.
    if !(-1e38..=1e38).contains(&biased) {
        return Err(EngineError::Unsupported(alloc::format!(
            "value {x} overflows NUMERIC range for column `{col_name}`"
        )));
    }
    let scaled = biased as i128;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// v7.17.0 Phase 3.P0-67 — parse PG-canonical decimal text into
/// `(mantissa: i128, source_scale: u8)`. Accepts optional sign,
/// optional integer part, optional fractional part. Rejects
/// scientific notation, embedded spaces, locale-specific
/// thousand separators. Returns None on bad input — coerce_value
/// turns that into a TypeMismatch error.
fn parse_numeric_text(s: &str) -> Option<(i128, u8)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (negative, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if rest.is_empty() {
        return None;
    }
    // Reject scientific notation — bigdecimal collapses it before
    // hitting the wire, and we want a clear error if a stray `e`
    // sneaks in.
    if rest.bytes().any(|b| b == b'e' || b == b'E') {
        return None;
    }
    let (int_part, frac_part) = match rest.find('.') {
        Some(idx) => (&rest[..idx], &rest[idx + 1..]),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if int_part.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    if frac_part.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    let scale_u32 = u32::try_from(frac_part.len()).ok()?;
    if scale_u32 > u32::from(u8::MAX) {
        return None;
    }
    let scale = scale_u32 as u8;
    let mut digits = alloc::string::String::with_capacity(int_part.len() + frac_part.len() + 1);
    if negative {
        digits.push('-');
    }
    digits.push_str(int_part);
    digits.push_str(frac_part);
    // Strip a leading "+0..0" so parse doesn't choke on "00" etc.
    let digits = if digits == "-" {
        return None;
    } else if digits.is_empty() {
        "0"
    } else {
        digits.as_str()
    };
    let mantissa: i128 = digits.parse().ok()?;
    Some((mantissa, scale))
}

/// Move a Numeric value from `src_scale` to `dst_scale`. Going up
/// multiplies by 10; going down rounds half-away-from-zero.
fn numeric_rescale(
    scaled: i128,
    src_scale: u8,
    precision: u8,
    dst_scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    let new_scaled = if dst_scale >= src_scale {
        let bump = pow10_i128(dst_scale - src_scale);
        scaled.checked_mul(bump).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "overflow rescaling NUMERIC for column `{col_name}`"
            ))
        })?
    } else {
        let drop = pow10_i128(src_scale - dst_scale);
        let half = drop / 2;
        if scaled >= 0 {
            (scaled + half) / drop
        } else {
            (scaled - half) / drop
        }
    };
    check_precision(new_scaled, precision, col_name)?;
    Ok(Value::Numeric {
        scaled: new_scaled,
        scale: dst_scale,
    })
}

/// Drop the fractional part of a scaled integer, returning the integer
/// portion (toward zero). Used for NUMERIC → INT casts.
const fn numeric_truncate_to_integer(scaled: i128, scale: u8) -> i128 {
    if scale == 0 {
        return scaled;
    }
    let factor = pow10_i128_const(scale);
    scaled / factor
}

/// Verify a scaled NUMERIC value fits the column's declared precision.
/// `precision == 0` is the "unconstrained" form (bare `NUMERIC`); we
/// skip the check there.
fn check_precision(scaled: i128, precision: u8, col_name: &str) -> Result<(), EngineError> {
    if precision == 0 {
        return Ok(());
    }
    let limit = pow10_i128(precision);
    if scaled.unsigned_abs() >= limit.unsigned_abs() {
        return Err(EngineError::Unsupported(alloc::format!(
            "NUMERIC value exceeds precision {precision} for column `{col_name}`"
        )));
    }
    Ok(())
}

const fn pow10_i128_const(p: u8) -> i128 {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        acc *= 10;
        i += 1;
    }
    acc
}

fn pow10_i128(p: u8) -> i128 {
    pow10_i128_const(p)
}

/// Walk a parsed `Statement`, swapping any `NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()` function calls for a
/// literal cast that wraps the engine's per-statement clock reading.
/// When `now_micros` is `None`, calls stay as-is and surface as
/// `unknown function` at eval time — keeps the error path explicit.
/// v4.10: pre-walk the WHERE / projection / etc. of a SELECT and
/// replace every subquery node with a materialised literal. SPG
/// only supports uncorrelated subqueries — the inner SELECT does
/// not see outer-row columns, so the result is the same for every
/// outer row and can be evaluated once.
///
/// Returns the rewritten statement; the caller passes this to the
/// regular row-loop executor which no longer sees Subquery nodes
/// in its tree.
impl Engine {
    /// v4.12 window executor. Implements `ROW_NUMBER` / `RANK` /
    /// `DENSE_RANK` and the partition-aware aggregates `SUM` /
    /// `AVG` / `COUNT` / `MIN` / `MAX`. The plan is:
    /// 1. Apply the WHERE filter.
    /// 2. For each unique `WindowFunction` node in the projection,
    ///    partition + sort, compute the per-row value.
    /// 3. Append the window values as synthetic columns (`__win_N`)
    ///    to the row schema.
    /// 4. Rewrite the projection to read those columns.
    /// 5. Hand off to the regular project / ORDER BY / LIMIT pipe.
    #[allow(
        clippy::too_many_lines,
        clippy::type_complexity,
        clippy::needless_range_loop
    )] // window-eval is one cohesive pipe; splitting fragments
    fn exec_select_with_window(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let from = stmt.from.as_ref().ok_or_else(|| {
            EngineError::Unsupported("window functions require a FROM clause".into())
        })?;
        // v7.17.0 Phase 3.P0-43 — JOIN + window functions. Phase
        // 3.6 rejected this combination outright ("queued for
        // v5.x"); P0-43 materialises the join + WHERE through the
        // existing nested-loop helper and runs the window pipeline
        // on the joined row set with the combined `alias.col`
        // schema. The window expressions resolve through the
        // qualifier-aware column resolver same as the aggregate /
        // projection paths on JOIN.
        let (schema_cols_owned, alias_opt): (Vec<ColumnSchema>, Option<&str>);
        let filtered: Vec<Row>;
        if from.joins.is_empty() {
            let primary = &from.primary;
            let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
                StorageError::TableNotFound {
                    name: primary.name.clone(),
                }
            })?;
            let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
            schema_cols_owned = table.schema().columns.clone();
            alias_opt = Some(alias);
            // Materialise WHERE-filtered rows owned so the JOIN
            // and single-table paths share a single downstream
            // shape. The clone is cheap relative to the window
            // computation that follows.
            let ctx = self.ev_ctx(&schema_cols_owned, alias_opt);
            let mut owned: Vec<Row> = Vec::new();
            for (i, row) in table.rows().iter().enumerate() {
                if i.is_multiple_of(256) {
                    cancel.check()?;
                }
                if let Some(w) = &stmt.where_ {
                    let cond = eval::eval_expr(w, row, &ctx)?;
                    if !matches!(cond, Value::Bool(true)) {
                        continue;
                    }
                }
                owned.push(row.clone());
            }
            filtered = owned;
        } else {
            let (combined_schema, rows) =
                self.build_joined_filtered_rows(from, stmt.where_.as_ref(), cancel, None)?;
            schema_cols_owned = combined_schema;
            alias_opt = None;
            filtered = rows;
        }
        let schema_cols = &schema_cols_owned;
        let ctx = self.ev_ctx(schema_cols, alias_opt);
        let alias = alias_opt.unwrap_or("");
        let n_rows = filtered.len();
        // Borrow refs into the owned row vec once so the downstream
        // `compute_window_partition` call (which takes `&[&Row]`) and
        // the per-row eval loops share a single backing buffer.
        let filtered_refs: Vec<&Row> = filtered.iter().collect();

        // 2) Collect unique window function nodes from projection.
        let mut window_nodes: Vec<Expr> = Vec::new();
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_window_nodes(expr, &mut window_nodes);
            }
        }

        // 3) For each window, compute per-row value.
        // Index: same order as window_nodes; for row i, win_vals[w][i].
        let mut win_vals: Vec<Vec<Value>> = Vec::with_capacity(window_nodes.len());
        for wnode in &window_nodes {
            let Expr::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
                null_treatment,
            } = wnode
            else {
                unreachable!("collect_window_nodes pushes only WindowFunction");
            };
            // Compute (partition_key, order_key, original_index) for each row.
            let mut indexed: Vec<(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)> =
                Vec::with_capacity(n_rows);
            for (i, row) in filtered.iter().enumerate() {
                let pkey: Vec<Value> = partition_by
                    .iter()
                    .map(|p| eval::eval_expr(p, row, &ctx))
                    .collect::<Result<_, _>>()?;
                let okey: Vec<(Value, bool, Option<bool>)> = order_by
                    .iter()
                    .map(|(e, desc, nf)| eval::eval_expr(e, row, &ctx).map(|v| (v, *desc, *nf)))
                    .collect::<Result<_, _>>()?;
                indexed.push((pkey, okey, i));
            }
            // Sort by (partition_key, order_key). Partition key uses
            // a stable encoded form; order key respects ASC/DESC.
            indexed.sort_by(|a, b| {
                let p_cmp = partition_key_cmp(&a.0, &b.0);
                if p_cmp != core::cmp::Ordering::Equal {
                    return p_cmp;
                }
                order_key_cmp(&a.1, &b.1)
            });
            // Per-partition compute.
            let mut out_vals: Vec<Value> = alloc::vec![Value::Null; n_rows];
            let mut p_start = 0;
            while p_start < indexed.len() {
                let mut p_end = p_start + 1;
                while p_end < indexed.len()
                    && partition_key_cmp(&indexed[p_start].0, &indexed[p_end].0)
                        == core::cmp::Ordering::Equal
                {
                    p_end += 1;
                }
                // Compute the function within this partition slice.
                compute_window_partition(
                    name,
                    args,
                    !order_by.is_empty(),
                    frame.as_ref(),
                    *null_treatment,
                    &indexed[p_start..p_end],
                    &filtered_refs,
                    &ctx,
                    &mut out_vals,
                )?;
                p_start = p_end;
            }
            win_vals.push(out_vals);
        }

        // 4) Build extended schema: original columns + synthetic.
        let mut ext_cols = schema_cols.clone();
        for i in 0..window_nodes.len() {
            ext_cols.push(ColumnSchema::new(
                alloc::format!("__win_{i}"),
                DataType::Text, // type doesn't matter for projection eval
                true,
            ));
        }
        // 5) Build extended rows: each row gets its window values appended.
        let mut ext_rows: Vec<Row> = Vec::with_capacity(n_rows);
        for i in 0..n_rows {
            let mut values = filtered[i].values.clone();
            for w in 0..window_nodes.len() {
                values.push(win_vals[w][i].clone());
            }
            ext_rows.push(Row::new(values));
        }
        // 6) Rewrite the projection: WindowFunction nodes → Column(__win_N).
        let mut rewritten_items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
        for item in &stmt.items {
            let new_item = match item {
                SelectItem::Wildcard => SelectItem::Wildcard,
                SelectItem::Expr { expr, alias } => {
                    let mut e = expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    SelectItem::Expr {
                        expr: e,
                        alias: alias.clone(),
                    }
                }
            };
            rewritten_items.push(new_item);
        }

        // 7) Project into final rows. JOIN case uses None so the
        // qualifier check in `resolve_column` falls through to the
        // composite `alias.col` schema lookup; single-table case
        // keeps the bare alias so `bare_col` resolution still
        // works for the projection's per-row column references.
        let ext_ctx = EvalContext::new(&ext_cols, alias_opt);
        let projection = build_projection(&rewritten_items, &ext_cols, alias)?;
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::with_capacity(n_rows);
        for (i, row) in ext_rows.iter().enumerate() {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ext_ctx)?);
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                let mut keys = Vec::with_capacity(stmt.order_by.len());
                for o in &stmt.order_by {
                    let mut e = o.expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    let key = eval::eval_expr(&e, row, &ext_ctx)?;
                    keys.push(value_to_order_key(&key)?);
                }
                keys
            };
            tagged.push((order_keys, Row::new(values)));
        }
        // ORDER BY + LIMIT/OFFSET on the projected rows.
        if !stmt.order_by.is_empty() {
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            sort_by_keys(&mut tagged, &descs);
        }
        let mut out_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        apply_offset_and_limit(&mut out_rows, stmt.offset_literal(), stmt.limit_literal());
        let final_cols: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(QueryResult::Rows {
            columns: final_cols,
            rows: out_rows,
        })
    }

    /// v4.11: materialise each CTE into a temp table inside a
    /// cloned catalog, then run the body SELECT against a fresh
    /// engine instance that owns the enriched catalog. The clone
    /// is moderately expensive — only paid by CTE-bearing queries.
    /// Subqueries inside CTE bodies / the main body resolve as
    /// usual; `clock_fn` is propagated so `NOW()` lines up.
    /// v7.16.2 — mailrs round-10 A.3. Materialise the
    /// `information_schema.*` / `pg_catalog.*` virtual views
    /// the SELECT references, then re-execute the SELECT
    /// against an enriched catalog where those views are real
    /// tables. Same pattern as `exec_with_ctes`. The temp
    /// engine carries `meta_views_materialised = true` so its
    /// own meta-dispatch short-circuits — without that we'd
    /// infinite-recurse since the temp catalog's view name
    /// still starts with `__spg_info_` and re-triggers the
    /// check.
    fn exec_select_with_meta_views(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let mut needed: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        collect_meta_view_names(stmt, &mut needed);
        let mut catalog = self.active_catalog().clone();
        for view in &needed {
            if catalog.get(view).is_some() {
                continue;
            }
            match view.as_str() {
                "__spg_info_columns" => {
                    let (schema, rows) = synth_information_schema_columns(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_info_tables" => {
                    let (schema, rows) = synth_information_schema_tables(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_class" => {
                    let (schema, rows) = synth_pg_class(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_attribute" => {
                    let (schema, rows) = synth_pg_attribute(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-50 — pg_catalog.pg_type for
                // sqlx / SQLAlchemy / Diesel / pgAdmin lookups.
                "__spg_pg_type" => {
                    let (schema, rows) = synth_pg_type(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-51 — pg_catalog.pg_proc for
                // function-name introspection (ORM / pgAdmin).
                "__spg_pg_proc" => {
                    let (schema, rows) = synth_pg_proc(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.24 (round-16 D) — pg_catalog.pg_trigger. The
                // round-16 "why doesn't prod fire the trigger"
                // question was unanswerable because triggers had NO
                // introspection surface; tgname/tgenabled plus the
                // pragmatic relname/timing/events/function columns
                // make "is it registered and enabled" a one-liner.
                "__spg_pg_trigger" => {
                    let (schema, rows) = synth_pg_trigger(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-52 — pg_catalog.pg_namespace
                // (schema list for admin tools' tree views).
                "__spg_pg_namespace" => {
                    let (schema, rows) = synth_pg_namespace(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-53 — pg_catalog.pg_indexes view
                // for pgAdmin / DataGrip "indexes per table" listings.
                "__spg_pg_indexes" => {
                    let (schema, rows) = synth_pg_indexes(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-53 — pg_catalog.pg_index (raw)
                // for index introspection by ORM compilers.
                "__spg_pg_index" => {
                    let (schema, rows) = synth_pg_index_raw(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-54 — pg_catalog.pg_constraint
                // for FK / UNIQUE / PK / CHECK introspection.
                "__spg_pg_constraint" => {
                    let (schema, rows) = synth_pg_constraint(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-55 — pg_catalog.pg_database /
                // pg_roles / pg_user. SPG is single-database so
                // pg_database surfaces just `postgres`; pg_roles
                // / pg_user walk the engine's UserStore.
                "__spg_pg_database" => {
                    let (schema, rows) = synth_pg_database(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_pg_roles" | "__spg_pg_user" => {
                    let (schema, rows) = synth_pg_roles(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-56 — pg_catalog.pg_views. PG's
                // pg_views surfaces every CREATE VIEW result; SPG
                // ships one row per declared view from the catalog.
                "__spg_pg_views" => {
                    let (schema, rows) = synth_pg_views(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-56 — pg_catalog.pg_matviews.
                // SPG has no materialised view surface yet so the
                // table shares pg_views's schema but stays empty.
                "__spg_pg_matviews" => {
                    let (schema, _) = synth_pg_views(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, Vec::new())?;
                }
                // pg_catalog.pg_extension — native capability list
                // (mailrs embed round-12).
                "__spg_pg_extension" => {
                    let (schema, rows) = synth_pg_extension();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-57 — pg_catalog.pg_settings.
                "__spg_pg_settings" => {
                    let (schema, rows) = synth_pg_settings(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-63 — information_schema.KEY_COLUMN_USAGE.
                "__spg_info_key_column_usage" => {
                    let (schema, rows) = synth_info_key_column_usage(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.REFERENTIAL_CONSTRAINTS.
                "__spg_info_referential_constraints" => {
                    let (schema, rows) = synth_info_referential_constraints(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.STATISTICS.
                "__spg_info_statistics" => {
                    let (schema, rows) = synth_info_statistics(self.active_catalog());
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-64 — information_schema.ROUTINES.
                "__spg_info_routines" => {
                    let (schema, rows) = synth_info_routines();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                // v7.17.0 Phase 3.P0-65 — mysql.user / mysql.db.
                "__spg_mysql_user" => {
                    let (schema, rows) = synth_mysql_user(self);
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                "__spg_mysql_db" => {
                    let (schema, rows) = synth_mysql_db();
                    materialise_meta_view(&mut catalog, view, schema, rows)?;
                }
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "meta view {view:?} is not yet materialisable; \
                         v7.16.2 covers information_schema.columns / .tables \
                         and pg_catalog.pg_class / pg_attribute; \
                         v7.17.0 P0-50..P0-57 add pg_type / pg_proc / pg_namespace / \
                         pg_indexes / pg_index / pg_constraint / pg_database / pg_roles / \
                         pg_user / pg_views / pg_matviews / pg_settings"
                    )));
                }
            }
        }
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        temp.meta_views_materialised = true;
        temp.exec_select_cancel(stmt, cancel)
    }

    fn exec_with_ctes(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut catalog = self.active_catalog().clone();
        for cte in &stmt.ctes {
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let (columns, rows) = if cte.recursive {
                self.materialise_recursive_cte(cte, &catalog, cancel)?
            } else {
                // v7.25 (round-17) — run the body against the
                // ACCUMULATED catalog so a CTE can reference every
                // CTE declared before it (`WITH a AS (…), b AS
                // (SELECT … FROM a)`). Executing on `self` lost the
                // already-materialised CTE tables.
                let mut cte_engine = Engine::restore(catalog.clone());
                if let Some(c) = self.clock {
                    cte_engine = cte_engine.with_clock(c);
                }
                if let Some(f) = self.salt_fn {
                    cte_engine = cte_engine.with_salt_fn(f);
                }
                let body_result = cte_engine.exec_select_cancel(&cte.body, cancel)?;
                let QueryResult::Rows { columns, rows } = body_result else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} body did not return rows",
                        cte.name
                    )));
                };
                (columns, rows)
            };
            // v4.22: the projection builder labels any non-column
            // expression as Text — including literal SELECT 1.
            // Promote each column's type to whatever the rows
            // actually carry so the CTE storage table accepts them.
            let inferred = infer_column_types(&columns, &rows);
            let mut columns = inferred;
            // v4.22: apply optional `WITH name(a, b, c)` overrides.
            if !cte.column_overrides.is_empty() {
                if cte.column_overrides.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} column list has {} names but body returns {} columns",
                        cte.name,
                        cte.column_overrides.len(),
                        columns.len()
                    )));
                }
                for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                    col.name.clone_from(name);
                }
            }
            let schema = TableSchema::new(cte.name.clone(), columns);
            catalog.create_table(schema).map_err(EngineError::Storage)?;
            let table = catalog
                .get_mut(&cte.name)
                .expect("just-created CTE table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
            }
        }
        // Strip CTEs from the body before running on the temp engine
        // so we don't recurse forever.
        let mut body = stmt.clone();
        body.ctes = Vec::new();
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        temp.exec_select_cancel(&body, cancel)
    }

    /// v4.22: materialise a WITH RECURSIVE CTE. The body must be a
    /// UNION (or UNION ALL) of an anchor that does not reference
    /// the CTE name, and one or more recursive terms that do. The
    /// anchor runs first; each subsequent iteration runs the
    /// recursive term against a temp catalog where the CTE name is
    /// bound to the *previous* iteration's output. Iteration stops
    /// when the recursive term yields no rows; UNION (DISTINCT)
    /// deduplicates against the accumulated result, UNION ALL does
    /// not. A hard cap on total rows prevents runaway queries.
    #[allow(clippy::too_many_lines)]
    fn materialise_recursive_cte(
        &self,
        cte: &spg_sql::ast::Cte,
        base_catalog: &Catalog,
        cancel: CancelToken<'_>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row>), EngineError> {
        const MAX_TOTAL_ROWS: usize = 1_000_000;
        const MAX_ITERATIONS: usize = 100_000;
        cancel.check()?;
        if cte.body.unions.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?} body must be a UNION of an anchor and a recursive term",
                cte.name
            )));
        }
        // Anchor: the body's leading SELECT, with unions stripped.
        let mut anchor = cte.body.clone();
        let union_terms = core::mem::take(&mut anchor.unions);
        anchor.ctes = Vec::new();
        // Anchor must not reference the CTE name.
        if select_refers_to(&anchor, &cte.name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: the anchor must not reference the CTE itself",
                cte.name
            )));
        }
        let anchor_result = self.exec_select_cancel(&anchor, cancel)?;
        let QueryResult::Rows {
            columns: anchor_cols,
            rows: anchor_rows,
        } = anchor_result
        else {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: anchor did not return rows",
                cte.name
            )));
        };
        // The projection builder labels non-column expressions Text;
        // refine column types from the anchor's actual values so the
        // intermediate iter-catalog tables accept them.
        let mut columns = infer_column_types(&anchor_cols, &anchor_rows);
        if !cte.column_overrides.is_empty() {
            if cte.column_overrides.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE {:?} column list has {} names but anchor returns {} columns",
                    cte.name,
                    cte.column_overrides.len(),
                    columns.len()
                )));
            }
            for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                col.name.clone_from(name);
            }
        }
        let mut all_rows: Vec<Row> = anchor_rows.clone();
        let mut working_set: Vec<Row> = anchor_rows;
        let mut seen: alloc::collections::BTreeSet<Vec<u8>> = alloc::collections::BTreeSet::new();
        // Track at least one "all UNION ALL" flag — if every union
        // kind is ALL we skip the dedup step (faster + matches PG).
        let all_union_all = union_terms.iter().all(|(k, _)| matches!(k, UnionKind::All));
        if !all_union_all {
            for r in &all_rows {
                seen.insert(encode_row_key(r));
            }
        }
        for iter in 0..MAX_ITERATIONS {
            cancel.check()?;
            if working_set.is_empty() {
                break;
            }
            // Build a fresh catalog: base + CTE bound to working_set.
            let mut iter_catalog = base_catalog.clone();
            let schema = TableSchema::new(cte.name.clone(), columns.clone());
            iter_catalog
                .create_table(schema)
                .map_err(EngineError::Storage)?;
            {
                let table = iter_catalog.get_mut(&cte.name).expect("just-created");
                for row in &working_set {
                    table.insert(row.clone()).map_err(EngineError::Storage)?;
                }
            }
            let mut iter_engine = Engine::restore(iter_catalog);
            if let Some(c) = self.clock {
                iter_engine = iter_engine.with_clock(c);
            }
            if let Some(f) = self.salt_fn {
                iter_engine = iter_engine.with_salt_fn(f);
            }
            // Run each recursive term in sequence and collect new rows.
            let mut next_set: Vec<Row> = Vec::new();
            for (_, term) in &union_terms {
                let mut term = term.clone();
                term.ctes = Vec::new();
                let r = iter_engine.exec_select_cancel(&term, cancel)?;
                let QueryResult::Rows {
                    columns: rc,
                    rows: rs,
                } = r
                else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: recursive term did not return rows",
                        cte.name
                    )));
                };
                if rc.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: column count of recursive term ({}) does not match anchor ({})",
                        cte.name,
                        rc.len(),
                        columns.len()
                    )));
                }
                for row in rs {
                    if !all_union_all {
                        let key = encode_row_key(&row);
                        if !seen.insert(key) {
                            continue;
                        }
                    }
                    next_set.push(row);
                }
            }
            if next_set.is_empty() {
                break;
            }
            all_rows.extend(next_set.iter().cloned());
            working_set = next_set;
            if all_rows.len() > MAX_TOTAL_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: produced more than {MAX_TOTAL_ROWS} rows — likely runaway recursion",
                    cte.name
                )));
            }
            if iter + 1 == MAX_ITERATIONS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: exceeded {MAX_ITERATIONS} iterations",
                    cte.name
                )));
            }
        }
        Ok((columns, all_rows))
    }

    fn resolve_select_subqueries(
        &self,
        stmt: &mut SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for item in &mut stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.resolve_expr_subqueries(expr, cancel)?;
            }
        }
        if let Some(w) = &mut stmt.where_ {
            self.resolve_expr_subqueries(w, cancel)?;
        }
        // v7.24.1 — JOIN ON conditions can carry subqueries too;
        // they were never walked, so even an UNCORRELATED subquery
        // in ON hit "subquery reached row eval".
        if let Some(from) = &mut stmt.from {
            for j in &mut from.joins {
                if let Some(on) = &mut j.on {
                    self.resolve_expr_subqueries(on, cancel)?;
                }
            }
        }
        if let Some(gs) = &mut stmt.group_by {
            for g in gs {
                self.resolve_expr_subqueries(g, cancel)?;
            }
        }
        if let Some(h) = &mut stmt.having {
            self.resolve_expr_subqueries(h, cancel)?;
        }
        for o in &mut stmt.order_by {
            self.resolve_expr_subqueries(&mut o.expr, cancel)?;
        }
        for (_, peer) in &mut stmt.unions {
            self.resolve_select_subqueries(peer, cancel)?;
        }
        Ok(())
    }

    #[allow(clippy::only_used_in_recursion)] // engine handle reads aren't really pure
    fn resolve_expr_subqueries(
        &self,
        e: &mut Expr,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        // Replace-on-this-node cases first.
        if let Some(replacement) = self.subquery_replacement(e, cancel)? {
            *e = replacement;
            return Ok(());
        }
        match e {
            Expr::AggregateOrdered { call, order_by, .. } => {
                self.resolve_expr_subqueries(call, cancel)?;
                for o in order_by.iter_mut() {
                    self.resolve_expr_subqueries(&mut o.expr, cancel)?;
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr_subqueries(lhs, cancel)?;
                self.resolve_expr_subqueries(rhs, cancel)?;
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                self.resolve_expr_subqueries(pattern, cancel)?;
            }
            Expr::Extract { source, .. } => self.resolve_expr_subqueries(source, cancel)?,
            // v4.12 window functions — recurse into args + ORDER BY
            // + PARTITION BY in case they carry inner subqueries.
            Expr::WindowFunction {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
                for p in partition_by {
                    self.resolve_expr_subqueries(p, cancel)?;
                }
                for (e, _, _) in order_by {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
            }
            // Subquery nodes are handled in subquery_replacement
            // (which returned None — defensive no-op); Literal /
            // Column are leaves.
            Expr::ScalarSubquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::Literal(_)
            | Expr::Placeholder(_)
            | Expr::Column(_) => {}
            // v7.10.10 — recurse children.
            Expr::Array(items) => {
                for elem in items {
                    self.resolve_expr_subqueries(elem, cancel)?;
                }
            }
            Expr::ArraySubscript { target, index } => {
                self.resolve_expr_subqueries(target, cancel)?;
                self.resolve_expr_subqueries(index, cancel)?;
            }
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                self.resolve_expr_subqueries(array, cancel)?;
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.resolve_expr_subqueries(o, cancel)?;
                }
                for (w, t) in branches {
                    self.resolve_expr_subqueries(w, cancel)?;
                    self.resolve_expr_subqueries(t, cancel)?;
                }
                if let Some(e) = else_branch {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
            }
        }
        Ok(())
    }

    /// v4.23: per-row eval that handles correlated subqueries.
    /// Equivalent to `eval::eval_expr` when the expression has no
    /// subqueries; otherwise clones the expression, substitutes
    /// outer-row columns into each surviving subquery node, runs
    /// the inner SELECT, and replaces the node with the literal
    /// result. Only the WHERE-filter call sites use this path so
    /// the uncorrelated fast path is preserved everywhere else.
    fn eval_expr_with_correlated(
        &self,
        expr: &Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<Value, EngineError> {
        if !expr_has_subquery(expr) {
            return eval::eval_expr(expr, row, ctx).map_err(EngineError::Eval);
        }
        // v7.29 (3c) - per-expression plan: the batch maps for this
        // host expression's scalar subqueries are looked up by the
        // expression's ADDRESS (stable across the row loop), so the
        // hot path does zero AST formatting. Building the plan (and
        // its Display-keyed group maps) happens once per expression.
        if let Some(m) = memo.as_deref_mut() {
            let key = core::ptr::from_ref::<Expr>(expr) as usize;
            // Plan hit: skip the collection walk entirely (it ran
            // once per group otherwise - 70k walks per inbox query).
            // The memo is per-query and host expressions outlive it,
            // so an address that hit once stays valid.
            let plan_hit = m.expr_plans.contains_key(&key);
            let mut subs: Vec<&SelectStatement> = Vec::new();
            if !plan_hit {
                collect_scalar_subqueries(expr, &mut subs);
            }
            if !plan_hit && !subs.is_empty() {
                let mut plan: Vec<Option<alloc::rc::Rc<memoize::GroupMap>>> =
                    Vec::with_capacity(subs.len());
                for sub in &subs {
                    let repr = alloc::format!("{sub}");
                    if !m.group_maps.contains_key(&repr) {
                        let built = self
                            .try_batch_correlated_scalar(sub, cancel)?
                            .map(alloc::rc::Rc::new);
                        m.group_maps.insert(repr.clone(), built);
                    }
                    plan.push(m.group_maps.get(&repr).cloned().flatten());
                }
                let mut template = expr.clone();
                hollow_scalar_subqueries(&mut template);
                m.expr_plans.insert(key, (subs.len(), plan, template));
            }
            if let Some((_, plan, template)) = m.expr_plans.get(&key)
                && !plan.is_empty()
                && plan.iter().all(|p| p.is_some())
            {
                // Fast path: every scalar subquery resolves via its
                // map; clone the HOLLOW template (subquery bodies
                // emptied at plan time - cloning full subquery ASTs
                // per row was the dominant malloc load), splice map
                // values, eval. Exists/IN subqueries (if any) still
                // drop to the resolver.
                let plan = plan.clone();
                let mut e = template.clone();
                let mut idx = 0usize;
                let ok = splice_planned_subqueries(&mut e, &plan, &mut idx, row, ctx)?;
                if ok {
                    if expr_has_subquery(&e) {
                        self.resolve_correlated_in_expr(&mut e, row, ctx, cancel, memo)?;
                    }
                    return eval::eval_expr(&e, row, ctx).map_err(EngineError::Eval);
                }
            }
        }
        let mut e = expr.clone();
        self.resolve_correlated_in_expr(&mut e, row, ctx, cancel, memo)?;
        eval::eval_expr(&e, row, ctx).map_err(EngineError::Eval)
    }

    fn resolve_correlated_in_expr(
        &self,
        e: &mut Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<(), EngineError> {
        match e {
            Expr::AggregateOrdered { call, order_by, .. } => {
                self.resolve_correlated_in_expr(call, row, ctx, cancel, memo.as_deref_mut())?;
                for o in order_by.iter_mut() {
                    self.resolve_correlated_in_expr(
                        &mut o.expr,
                        row,
                        ctx,
                        cancel,
                        memo.as_deref_mut(),
                    )?;
                }
            }
            Expr::ScalarSubquery(inner) => {
                // v7.29 (round-22 phase 3) — batch path first: a
                // correlated scalar of the `inner_col = outer_col
                // [ORDER BY … LIMIT 1]` shape evaluates ONCE as a
                // grouped scan; per-row resolution becomes a map
                // lookup. 23.5k per-group executions (~900 ms) became
                // one scan + lookups.
                if memo.is_some() {
                    let repr = alloc::format!("{}", **inner);
                    let entry_known = memo
                        .as_ref()
                        .is_some_and(|m| m.group_maps.contains_key(&repr));
                    if !entry_known {
                        let built = self
                            .try_batch_correlated_scalar(inner, cancel)?
                            .map(alloc::rc::Rc::new);
                        if let Some(m) = memo.as_deref_mut() {
                            m.group_maps.insert(repr.clone(), built);
                        }
                    }
                    if let Some(m) = memo.as_deref_mut()
                        && let Some(Some(gm)) = m.group_maps.get(&repr)
                    {
                        let (outer_col, map) = gm.as_ref();
                        let key_v = eval::eval_expr(&Expr::Column(outer_col.clone()), row, ctx)
                            .map_err(EngineError::Eval)?;
                        let v = if matches!(key_v, Value::Null) {
                            Value::Null
                        } else {
                            map.get(&aggregate::encode_key(core::slice::from_ref(&key_v)))
                                .cloned()
                                .unwrap_or(Value::Null)
                        };
                        *e = value_to_literal_expr(v)?;
                        return Ok(());
                    }
                }
                // v6.2.6 — Memoize: build the cache key from the
                // pre-substitution subquery repr + the outer row's
                // values. Two outer rows with identical correlated
                // values hit the same entry.
                let cache_key = memo.as_ref().map(|_| memoize::CacheKey {
                    subquery_repr: alloc::format!("{}", **inner),
                    outer_values: row.values.clone(),
                });
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key.as_ref())
                    && let Some(cached) = cache.get(k)
                {
                    *e = value_to_literal_expr(cached)?;
                    return Ok(());
                }
                let mut s = (**inner).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [r0] => r0.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key) {
                    cache.insert(k, value.clone());
                }
                *e = value_to_literal_expr(value)?;
            }
            Expr::Exists { subquery, negated } => {
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let exists = matches!(r, QueryResult::Rows { rows, .. } if !rows.is_empty());
                let bit = if *negated { !exists } else { exists };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::InSubquery {
                expr: lhs,
                subquery,
                negated,
            } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel, memo.as_deref_mut())?;
                let lhs_val = eval::eval_expr(lhs, row, ctx).map_err(EngineError::Eval)?;
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                let mut found = false;
                let mut any_null = false;
                for r0 in rows {
                    let v = r0.values.into_iter().next().unwrap_or(Value::Null);
                    if v.is_null() {
                        any_null = true;
                        continue;
                    }
                    if value_cmp(&v, &lhs_val) == core::cmp::Ordering::Equal {
                        found = true;
                        break;
                    }
                }
                let bit = if found {
                    !*negated
                } else if any_null {
                    return Err(EngineError::Unsupported(
                        "IN-subquery with NULL in result and no match: NULL semantics not yet implemented".into(),
                    ));
                } else {
                    *negated
                };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(rhs, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(pattern, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_correlated_in_expr(a, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::Extract { source, .. } => {
                self.resolve_correlated_in_expr(source, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::WindowFunction { .. }
            | Expr::Literal(_)
            | Expr::Placeholder(_)
            | Expr::Column(_) => {}
            // v7.10.10 — recurse children.
            Expr::Array(items) => {
                for elem in items {
                    self.resolve_correlated_in_expr(elem, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::ArraySubscript { target, index } => {
                self.resolve_correlated_in_expr(target, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(index, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(array, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.resolve_correlated_in_expr(o, row, ctx, cancel, memo.as_deref_mut())?;
                }
                for (w, t) in branches {
                    self.resolve_correlated_in_expr(w, row, ctx, cancel, memo.as_deref_mut())?;
                    self.resolve_correlated_in_expr(t, row, ctx, cancel, memo.as_deref_mut())?;
                }
                if let Some(e) = else_branch {
                    self.resolve_correlated_in_expr(e, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
        }
        Ok(())
    }

    fn subquery_replacement(
        &self,
        e: &Expr,
        cancel: CancelToken<'_>,
    ) -> Result<Option<Expr>, EngineError> {
        match e {
            Expr::ScalarSubquery(inner) => {
                let mut s = (**inner).clone();
                // Recurse into the inner SELECT first so nested
                // subqueries materialise bottom-up.
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner statement did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [row] => row.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                Ok(Some(value_to_literal_expr(value)?))
            }
            Expr::Exists { subquery, negated } => {
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let exists = match r {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    QueryResult::CommandOk { .. } => false,
                };
                let bit = if *negated { !exists } else { exists };
                Ok(Some(Expr::Literal(Literal::Bool(bit))))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner statement did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                // Build the same OR-Eq chain the parse-time literal-list
                // path constructs, with each value lifted into a Literal.
                let mut acc: Option<Expr> = None;
                for row in rows {
                    let v = row.values.into_iter().next().unwrap_or(Value::Null);
                    let lit = value_to_literal_expr(v)?;
                    let cmp = Expr::Binary {
                        lhs: expr.clone(),
                        op: BinOp::Eq,
                        rhs: Box::new(lit),
                    };
                    acc = Some(match acc {
                        None => cmp,
                        Some(prev) => Expr::Binary {
                            lhs: Box::new(prev),
                            op: BinOp::Or,
                            rhs: Box::new(cmp),
                        },
                    });
                }
                let combined = acc.unwrap_or(Expr::Literal(Literal::Bool(false)));
                let final_expr = if *negated {
                    Expr::Unary {
                        op: UnOp::Not,
                        expr: Box::new(combined),
                    }
                } else {
                    combined
                };
                Ok(Some(final_expr))
            }
            _ => Ok(None),
        }
    }
}

// ---- v4.12 window-function helpers ----
// The (partition-key, order-key, original-index) tuple shape used
// across these helpers is intrinsic to the planner. Factoring it
// into a typedef adds indirection without making the code clearer,
// so several lints are allowed inline on the affected functions
// rather than module-wide.

/// v4.22: cheap structural scan for `FROM <name>` (qualified or
/// not) inside a SELECT — used to verify the anchor of a WITH
/// RECURSIVE CTE doesn't recurse into itself. Conservative: walks
/// FROM joins, subqueries, and unions.
fn select_refers_to(stmt: &SelectStatement, target: &str) -> bool {
    if let Some(from) = &stmt.from
        && from_refers_to(from, target)
    {
        return true;
    }
    for (_, peer) in &stmt.unions {
        if select_refers_to(peer, target) {
            return true;
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_refers_to(expr, target)
        {
            return true;
        }
    }
    if let Some(w) = &stmt.where_
        && expr_refers_to(w, target)
    {
        return true;
    }
    false
}

fn from_refers_to(from: &FromClause, target: &str) -> bool {
    if from.primary.name.eq_ignore_ascii_case(target) {
        return true;
    }
    from.joins
        .iter()
        .any(|j| j.table.name.eq_ignore_ascii_case(target))
}

/// v7.28 (round-22) — collect every QUALIFIED column referenced
/// anywhere in a SELECT (subquery bodies included). Returns None
/// when a wildcard or a bare column name makes static attribution
/// unsafe — callers then keep every column.
fn collect_qualified_refs(
    stmt: &SelectStatement,
    out: &mut alloc::collections::BTreeSet<(String, String)>,
) -> Option<()> {
    for item in &stmt.items {
        match item {
            SelectItem::Wildcard => return None,
            SelectItem::Expr { expr, .. } => collect_qualified_refs_expr(expr, out)?,
        }
    }
    if let Some(w) = &stmt.where_ {
        collect_qualified_refs_expr(w, out)?;
    }
    if let Some(from) = &stmt.from {
        for j in &from.joins {
            if let Some(on) = &j.on {
                collect_qualified_refs_expr(on, out)?;
            }
            if j.table.lateral_subquery.is_some() {
                return None;
            }
        }
    }
    if let Some(gs) = &stmt.group_by {
        for g in gs {
            collect_qualified_refs_expr(g, out)?;
        }
    }
    if let Some(h) = &stmt.having {
        collect_qualified_refs_expr(h, out)?;
    }
    for o in &stmt.order_by {
        collect_qualified_refs_expr(&o.expr, out)?;
    }
    for (_, peer) in &stmt.unions {
        collect_qualified_refs(peer, out)?;
    }
    for cte in &stmt.ctes {
        collect_qualified_refs(&cte.body, out)?;
    }
    Some(())
}

fn collect_qualified_refs_expr(
    e: &Expr,
    out: &mut alloc::collections::BTreeSet<(String, String)>,
) -> Option<()> {
    // Two passes so the column and subquery visitors don't both
    // capture `out` mutably.
    let mut cols: Vec<spg_sql::ast::ColumnName> = Vec::new();
    let mut subs: Vec<&SelectStatement> = Vec::new();
    visit_expr_columns_and_subqueries(
        e,
        &mut |c: &spg_sql::ast::ColumnName| cols.push(c.clone()),
        &mut |sub| subs.push(sub),
    );
    for c in cols {
        match c.qualifier {
            Some(q) => {
                out.insert((q, c.name));
            }
            None => return None,
        }
    }
    for sub in subs {
        collect_qualified_refs(sub, out)?;
    }
    Some(())
}

/// Immutable walk over an Expr visiting every Column and every
/// nested SelectStatement (v7.28).
fn visit_expr_columns_and_subqueries<'a>(
    e: &'a Expr,
    on_col: &mut impl FnMut(&'a spg_sql::ast::ColumnName),
    on_sub: &mut impl FnMut(&'a SelectStatement),
) {
    match e {
        Expr::Column(c) => on_col(c),
        Expr::ScalarSubquery(s) => on_sub(s),
        Expr::Exists { subquery, .. } => on_sub(subquery),
        Expr::InSubquery { expr, subquery, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
            on_sub(subquery);
        }
        Expr::Binary { lhs, rhs, .. } => {
            visit_expr_columns_and_subqueries(lhs, on_col, on_sub);
            visit_expr_columns_and_subqueries(rhs, on_col, on_sub);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
        }
        Expr::Like { expr, pattern, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
            visit_expr_columns_and_subqueries(pattern, on_col, on_sub);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                visit_expr_columns_and_subqueries(a, on_col, on_sub);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            visit_expr_columns_and_subqueries(call, on_col, on_sub);
            for o in order_by {
                visit_expr_columns_and_subqueries(&o.expr, on_col, on_sub);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                visit_expr_columns_and_subqueries(op, on_col, on_sub);
            }
            for (w, t) in branches {
                visit_expr_columns_and_subqueries(w, on_col, on_sub);
                visit_expr_columns_and_subqueries(t, on_col, on_sub);
            }
            if let Some(eb) = else_branch {
                visit_expr_columns_and_subqueries(eb, on_col, on_sub);
            }
        }
        Expr::ArraySubscript { target, index } => {
            visit_expr_columns_and_subqueries(target, on_col, on_sub);
            visit_expr_columns_and_subqueries(index, on_col, on_sub);
        }
        Expr::Literal(_) | Expr::Placeholder(_) => {}
        // Exotic nodes (window etc.) — visit nothing extra; their
        // columns are caught when the caller bails on bare names
        // elsewhere, and window queries skip pruning entirely at
        // the call sites.
        _ => {
            // Exotic node (window function etc.): report an
            // unattributable marker so callers disable pruning.
            static BAIL: spg_sql::ast::ColumnName = spg_sql::ast::ColumnName {
                qualifier: None,
                name: String::new(),
            };
            on_col(&BAIL);
        }
    }
}

/// v7.28 (round-22) — collect every Column qualifier in an expr;
/// `all_qualified` flips false on any bare column (those can't be
/// attributed to one table safely, so the pushdown skips them).
fn collect_column_qualifiers<'e>(e: &'e Expr, out: &mut Vec<&'e str>, all_qualified: &mut bool) {
    if let Expr::Column(c) = e {
        match &c.qualifier {
            Some(q) => out.push(q.as_str()),
            None => *all_qualified = false,
        }
        return;
    }
    // Reuse the canonical immutable walk via describe's walker shape:
    // recurse the common containers.
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            collect_column_qualifiers(lhs, out, all_qualified);
            collect_column_qualifiers(rhs, out, all_qualified);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_column_qualifiers(expr, out, all_qualified);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_column_qualifiers(expr, out, all_qualified);
            collect_column_qualifiers(pattern, out, all_qualified);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_column_qualifiers(a, out, all_qualified);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) => {}
        // Anything exotic (CASE, subquery, window, arrays…):
        // conservatively mark unattributable.
        _ => *all_qualified = false,
    }
}

fn expr_refers_to(e: &Expr, target: &str) -> bool {
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_refers_to(call, target) || order_by.iter().any(|o| expr_refers_to(&o.expr, target))
        }
        Expr::ScalarSubquery(s) => select_refers_to(s, target),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            select_refers_to(subquery, target)
        }
        Expr::Binary { lhs, rhs, .. } => expr_refers_to(lhs, target) || expr_refers_to(rhs, target),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_refers_to(expr, target)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_refers_to(expr, target) || expr_refers_to(pattern, target)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(|a| expr_refers_to(a, target)),
        Expr::Extract { source, .. } => expr_refers_to(source, target),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(|a| expr_refers_to(a, target))
                || partition_by.iter().any(|p| expr_refers_to(p, target))
                || order_by.iter().any(|(o, _, _)| expr_refers_to(o, target))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(|e| expr_refers_to(e, target)),
        Expr::ArraySubscript { target: t, index } => {
            expr_refers_to(t, target) || expr_refers_to(index, target)
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_refers_to(expr, target) || expr_refers_to(array, target)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand
                .as_deref()
                .is_some_and(|o| expr_refers_to(o, target))
                || branches
                    .iter()
                    .any(|(w, t)| expr_refers_to(w, target) || expr_refers_to(t, target))
                || else_branch
                    .as_deref()
                    .is_some_and(|e| expr_refers_to(e, target))
        }
    }
}

/// v4.22: pick more specific column types from observed rows when
/// the projection builder defaulted to Text (the v1.x behavior for
/// non-column expressions). Lets `WITH t(n) AS (SELECT 1 ...)`
/// land an Int column in the CTE storage table rather than failing
/// the insert with "expected TEXT, got INT".
/// v7.16.2 — map an SPG [`DataType`] to the PG-canonical
/// `information_schema.columns.data_type` text. Covers the
/// values mailrs's migrations probe (`'ARRAY'`, `'integer'`,
/// `'text'`, …). Unknown variants fall back to the SPG name
/// downcased — better than panicking on a future DataType.
fn pg_data_type_text(ty: DataType) -> alloc::string::String {
    let s = match ty {
        DataType::Int => "integer",
        DataType::BigInt => "bigint",
        DataType::SmallInt => "smallint",
        DataType::Float => "double precision",
        DataType::Bool => "boolean",
        DataType::Text => "text",
        DataType::Varchar(_) => "character varying",
        DataType::Date => "date",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Timestamptz => "timestamp with time zone",
        DataType::Json => "jsonb",
        DataType::Bytes => "bytea",
        DataType::TextArray | DataType::IntArray | DataType::BigIntArray => "ARRAY",
        DataType::TsVector => "tsvector",
        DataType::TsQuery => "tsquery",
        DataType::Vector { .. } => "USER-DEFINED",
        // Non-exhaustive — fall back to "USER-DEFINED" the way
        // PG labels any pg_type it doesn't recognise.
        _ => "USER-DEFINED",
    };
    alloc::string::String::from(s)
}

/// v7.16.2 — synthesise `information_schema.columns`. mailrs
/// queries are of shape `SELECT 1 FROM information_schema.columns
/// WHERE table_name = … AND column_name = … AND data_type = …` —
/// the v7.16.2 view returns the columns mailrs probes; broader
/// PG-spec parity (ordinal_position, is_nullable, character_
/// maximum_length, udt_name, …) lands as needed.
fn synth_information_schema_columns(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("is_nullable", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(Row::new(alloc::vec![
                Value::Text("spg".into()),
                Value::Text("public".into()),
                Value::Text(tname.clone()),
                Value::Text(col.name.clone()),
                Value::Int(ordinal),
                Value::Text(if col.nullable {
                    "YES".into()
                } else {
                    "NO".into()
                }),
                Value::Text(pg_data_type_text(col.ty)),
            ]));
        }
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `information_schema.tables`.
fn synth_information_schema_tables(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("table_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        rows.push(Row::new(alloc::vec![
            Value::Text("spg".into()),
            Value::Text("public".into()),
            Value::Text(tname.clone()),
            Value::Text("BASE TABLE".into()),
        ]));
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `pg_catalog.pg_class`. Minimum shape
/// for psql `\d` / ORM probes: `relname` + `relkind`. Each
/// user table emits one row.
fn synth_pg_class(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("relkind", DataType::Text, false),
        ColumnSchema::new("relnamespace", DataType::BigInt, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        rows.push(Row::new(alloc::vec![
            Value::Text(tname.clone()),
            Value::Text("r".into()),
            Value::BigInt(2200), // PG's `public` namespace OID
        ]));
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `pg_catalog.pg_attribute`. Minimum
/// shape: `attrelid` (text — SPG has no OID), `attname`,
/// `attnum`, `atttypid` (text), `attnotnull`.
fn synth_pg_attribute(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("attrelid", DataType::Text, false),
        ColumnSchema::new("attname", DataType::Text, false),
        ColumnSchema::new("attnum", DataType::Int, false),
        ColumnSchema::new("atttypid", DataType::Text, false),
        ColumnSchema::new("attnotnull", DataType::Bool, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(Row::new(alloc::vec![
                Value::Text(tname.clone()),
                Value::Text(col.name.clone()),
                Value::Int(ordinal),
                Value::Text(pg_data_type_text(col.ty)),
                Value::Bool(!col.nullable),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-50 — synthesise `pg_catalog.pg_type`. The
/// returned rows cover every built-in scalar / array type sqlx,
/// SQLAlchemy, Diesel and pgAdmin look up at compile / connect
/// time. PG-canonical schema columns we expose:
///   * oid           — type OID (the lookup key sqlx uses)
///   * typname       — canonical type name (`int4`, `text`, …)
///   * typlen        — width in bytes (-1 for var-length)
///   * typtype       — `b`ase / `c`omposite / `e`num / etc.
///   * typcategory   — PG type category single-char
///   * typelem       — element OID for arrays (0 otherwise)
///   * typarray      — array-type OID (0 if no array type)
///   * typnamespace  — schema OID (always `public` = 2200)
///
/// Other pg_type columns (typowner, typinput/typoutput, etc.)
/// land in follow-up work — sqlx encoders don't query them at
/// connect time.
fn synth_pg_type(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("typname", DataType::Text, false),
        ColumnSchema::new("typlen", DataType::SmallInt, false),
        ColumnSchema::new("typtype", DataType::Text, false),
        ColumnSchema::new("typcategory", DataType::Text, false),
        ColumnSchema::new("typelem", DataType::BigInt, false),
        ColumnSchema::new("typarray", DataType::BigInt, false),
        ColumnSchema::new("typnamespace", DataType::BigInt, false),
    ];
    // (oid, name, len, type, cat, elem, array_oid). PG OID
    // numbers come straight from `pg_type.dat`.
    let scalars: &[(i64, &str, i16, &str, &str, i64, i64)] = &[
        // bool
        (16, "bool", 1, "b", "B", 0, 1000),
        (17, "bytea", -1, "b", "U", 0, 1001),
        (18, "char", 1, "b", "S", 0, 1002),
        (19, "name", 64, "b", "S", 0, 1003),
        (20, "int8", 8, "b", "N", 0, 1016),
        (21, "int2", 2, "b", "N", 0, 1005),
        (23, "int4", 4, "b", "N", 0, 1007),
        (24, "regproc", 4, "b", "N", 0, 1008),
        (25, "text", -1, "b", "S", 0, 1009),
        (26, "oid", 4, "b", "N", 0, 1028),
        (114, "json", -1, "b", "U", 0, 199),
        (142, "xml", -1, "b", "U", 0, 143),
        (700, "float4", 4, "b", "N", 0, 1021),
        (701, "float8", 8, "b", "N", 0, 1022),
        (650, "cidr", -1, "b", "I", 0, 651),
        (869, "inet", -1, "b", "I", 0, 1041),
        (829, "macaddr", 6, "b", "U", 0, 1040),
        (1042, "bpchar", -1, "b", "S", 0, 1014),
        (1043, "varchar", -1, "b", "S", 0, 1015),
        (1082, "date", 4, "b", "D", 0, 1182),
        (1083, "time", 8, "b", "D", 0, 1183),
        (1114, "timestamp", 8, "b", "D", 0, 1115),
        (1184, "timestamptz", 8, "b", "D", 0, 1185),
        (1186, "interval", 16, "b", "T", 0, 1187),
        (1266, "timetz", 12, "b", "D", 0, 1270),
        (1700, "numeric", -1, "b", "N", 0, 1231),
        (790, "money", 8, "b", "N", 0, 791),
        (2950, "uuid", 16, "b", "U", 0, 2951),
        (3802, "jsonb", -1, "b", "U", 0, 3807),
        (3614, "tsvector", -1, "b", "U", 0, 3643),
        (3615, "tsquery", -1, "b", "U", 0, 3645),
        // hstore + range types — typcategory 'U' (user) / 'R' (range).
        (3908, "tstzrange", -1, "r", "R", 0, 3909),
        (3910, "tsrange", -1, "r", "R", 0, 3911),
        (3904, "int4range", -1, "r", "R", 0, 3905),
        (3926, "int8range", -1, "r", "R", 0, 3927),
        (3906, "numrange", -1, "r", "R", 0, 3907),
        (3912, "daterange", -1, "r", "R", 0, 3913),
    ];
    // Array companion types share the typelem / typcategory='A'.
    // We emit just the array OIDs the scalars reference.
    let arrays: &[(i64, &str, i64)] = &[
        (1000, "_bool", 16),
        (1001, "_bytea", 17),
        (1002, "_char", 18),
        (1003, "_name", 19),
        (1016, "_int8", 20),
        (1005, "_int2", 21),
        (1007, "_int4", 23),
        (1008, "_regproc", 24),
        (1009, "_text", 25),
        (1028, "_oid", 26),
        (199, "_json", 114),
        (143, "_xml", 142),
        (1021, "_float4", 700),
        (1022, "_float8", 701),
        (651, "_cidr", 650),
        (1041, "_inet", 869),
        (1040, "_macaddr", 829),
        (1014, "_bpchar", 1042),
        (1015, "_varchar", 1043),
        (1182, "_date", 1082),
        (1183, "_time", 1083),
        (1115, "_timestamp", 1114),
        (1185, "_timestamptz", 1184),
        (1187, "_interval", 1186),
        (1270, "_timetz", 1266),
        (1231, "_numeric", 1700),
        (791, "_money", 790),
        (2951, "_uuid", 2950),
        (3807, "_jsonb", 3802),
        (3643, "_tsvector", 3614),
        (3645, "_tsquery", 3615),
    ];
    let mut rows: Vec<Row> = Vec::with_capacity(scalars.len() + arrays.len());
    for &(oid, name, len, ty, cat, elem, arr) in scalars {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::Text(name.into()),
            Value::SmallInt(len),
            Value::Text(ty.into()),
            Value::Text(cat.into()),
            Value::BigInt(elem),
            Value::BigInt(arr),
            Value::BigInt(2200),
        ]));
    }
    for &(oid, name, elem) in arrays {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::Text(name.into()),
            Value::SmallInt(-1),
            Value::Text("b".into()),
            Value::Text("A".into()),
            Value::BigInt(elem),
            Value::BigInt(0),
            Value::BigInt(2200),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-51 — synthesise `pg_catalog.pg_proc`. ORM /
/// pgAdmin probes look up functions by name; SPG synthesises rows
/// for the built-in scalar functions / aggregates / window funcs
/// the engine actually dispatches. SPG has no user-defined
/// functions yet so the table is a stable static list.
///
/// Schema columns exposed:
///   * oid (BigInt) — function OID from PG's pg_proc.dat
///   * proname (Text) — function name (lowercase)
///   * pronamespace (BigInt) — 11 (`pg_catalog`)
///   * prokind (Text) — 'f' function, 'a' aggregate, 'w' window
///   * pronargs (SmallInt) — declared arg count (-1 for variadic)
///   * prorettype (BigInt) — return type OID (matches synth_pg_type)
/// v7.24 (round-16 D) — synthesise `pg_catalog.pg_trigger` from the
/// live catalog. PG-shaped core columns (tgname, tgenabled with
/// 'O'/'D') plus pragmatic text columns PG keeps relational
/// (relname, timing, events, function) so health checks don't need
/// oid joins.
fn synth_pg_trigger(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("tgname", DataType::Text, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("tgenabled", DataType::Text, false),
        ColumnSchema::new("timing", DataType::Text, false),
        ColumnSchema::new("events", DataType::Text, false),
        ColumnSchema::new("function", DataType::Text, false),
    ];
    let rows: Vec<Row> = cat
        .triggers()
        .iter()
        .map(|t| {
            Row::new(alloc::vec![
                Value::Text(t.name.clone()),
                Value::Text(t.table.clone()),
                Value::Text(if t.enabled { "O".into() } else { "D".into() }),
                Value::Text(t.timing.clone()),
                Value::Text(t.events.join(" OR ")),
                Value::Text(t.function.clone()),
            ])
        })
        .collect();
    (schema, rows)
}

fn synth_pg_proc(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("proname", DataType::Text, false),
        ColumnSchema::new("pronamespace", DataType::BigInt, false),
        ColumnSchema::new("prokind", DataType::Text, false),
        ColumnSchema::new("pronargs", DataType::Int, false),
        ColumnSchema::new("prorettype", DataType::BigInt, false),
    ];
    // (oid, name, kind, nargs, rettype). OIDs taken from PG's
    // pg_proc.dat for the common subset.
    let funcs: &[(i64, &str, &str, i32, i64)] = &[
        // Scalar functions.
        (1318, "length", "f", 1, 23),
        (871, "upper", "f", 1, 25),
        (870, "lower", "f", 1, 25),
        (936, "substring", "f", 3, 25),
        (937, "substring", "f", 2, 25),
        (3055, "btrim", "f", 1, 25),
        (885, "btrim", "f", 2, 25),
        (3056, "ltrim", "f", 1, 25),
        (875, "ltrim", "f", 2, 25),
        (3057, "rtrim", "f", 1, 25),
        (876, "rtrim", "f", 2, 25),
        (1397, "abs", "f", 1, 23),
        (1396, "abs", "f", 1, 20),
        (1606, "round", "f", 1, 1700),
        (1707, "round", "f", 2, 1700),
        (2308, "ceil", "f", 1, 701),
        (2309, "ceiling", "f", 1, 701),
        (2310, "floor", "f", 1, 701),
        (1376, "sqrt", "f", 1, 701),
        (1369, "ln", "f", 1, 701),
        (1373, "exp", "f", 1, 701),
        (1368, "power", "f", 2, 701),
        (2228, "random", "f", 0, 701),
        // Date / time.
        (1299, "now", "f", 0, 1184),
        (1274, "current_timestamp", "f", 0, 1184),
        (1140, "current_date", "f", 0, 1082),
        (2050, "current_time", "f", 0, 1083),
        (1158, "date_trunc", "f", 2, 1184),
        (1171, "date_part", "f", 2, 701),
        (1172, "age", "f", 1, 1186),
        (936, "to_char", "f", 2, 25),
        // Session / introspection.
        (861, "current_database", "f", 0, 19),
        (745, "current_user", "f", 0, 19),
        (745, "session_user", "f", 0, 19),
        (1402, "current_schema", "f", 0, 19),
        // String concat / format.
        (3058, "concat", "f", -1, 25),
        (3059, "concat_ws", "f", -1, 25),
        (3539, "format", "f", -1, 25),
        // Type introspection.
        (2877, "pg_typeof", "f", 1, 2206),
        // JSON.
        (3198, "json_build_object", "f", -1, 114),
        (3199, "jsonb_build_object", "f", -1, 3802),
        (3271, "json_build_array", "f", -1, 114),
        (3272, "jsonb_build_array", "f", -1, 3802),
        // UUID.
        (3253, "gen_random_uuid", "f", 0, 2950),
        (3252, "uuid_generate_v4", "f", 0, 2950),
        // Aggregates.
        (2147, "count", "a", 0, 20),
        (2803, "count", "a", -1, 20),
        (2116, "max", "a", 1, 23),
        (2132, "min", "a", 1, 23),
        (2108, "sum", "a", 1, 20),
        (2100, "avg", "a", 1, 1700),
        (2517, "string_agg", "a", 2, 25),
        (2747, "array_agg", "a", 1, 1009),
        (2517, "bool_and", "a", 1, 16),
        (2518, "bool_or", "a", 1, 16),
        (2519, "every", "a", 1, 16),
        // Window functions.
        (3100, "row_number", "w", 0, 20),
        (3101, "rank", "w", 0, 20),
        (3102, "dense_rank", "w", 0, 20),
        (3103, "percent_rank", "w", 0, 701),
        (3104, "cume_dist", "w", 0, 701),
        (3105, "lag", "w", -1, 2283),
        (3106, "lead", "w", -1, 2283),
        (3107, "first_value", "w", 1, 2283),
        (3108, "last_value", "w", 1, 2283),
        (3109, "nth_value", "w", 2, 2283),
    ];
    let mut rows: Vec<Row> = Vec::with_capacity(funcs.len());
    for &(oid, name, kind, nargs, rettype) in funcs {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::Text(name.into()),
            Value::BigInt(11),
            Value::Text(kind.into()),
            Value::Int(nargs),
            Value::BigInt(rettype),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-65 — synthesise `mysql.user`. MySQL admin
/// queries (`SELECT user, host FROM mysql.user`) probe this at
/// connect time to list accounts. SPG ships one row per
/// UserStore entry plus a synthetic `root` superuser row for
/// MySQL bootstrap compat.
fn synth_mysql_user(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("user", DataType::Text, false),
        ColumnSchema::new("host", DataType::Text, false),
        ColumnSchema::new("select_priv", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    rows.push(Row::new(alloc::vec![
        Value::Text("root".into()),
        Value::Text("localhost".into()),
        Value::Text("Y".into()),
    ]));
    for (name, _) in engine.users.iter() {
        if name != "root" {
            rows.push(Row::new(alloc::vec![
                Value::Text(name.to_string()),
                Value::Text("%".into()),
                Value::Text("Y".into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-65 — synthesise `mysql.db`. The
/// per-database privileges table. SPG is single-database so the
/// table surfaces one row per declared user with full privileges
/// on the canonical `postgres` database.
fn synth_mysql_db() -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("host", DataType::Text, false),
        ColumnSchema::new("db", DataType::Text, false),
        ColumnSchema::new("user", DataType::Text, false),
        ColumnSchema::new("select_priv", DataType::Text, false),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::Text("localhost".into()),
        Value::Text("postgres".into()),
        Value::Text("root".into()),
        Value::Text("Y".into()),
    ])];
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-63 — synthesise
/// `information_schema.KEY_COLUMN_USAGE`. ORM migration tools
/// (Alembic, Sequelize, TypeORM) walk this view to discover FK
/// relationships in MySQL-flavoured introspection queries.
///
/// Schema columns exposed:
///   * CONSTRAINT_NAME (Text)
///   * TABLE_NAME (Text)
///   * COLUMN_NAME (Text)
///   * ORDINAL_POSITION (Int)
///   * REFERENCED_TABLE_NAME (Text) — empty for non-FK rows
///   * REFERENCED_COLUMN_NAME (Text) — empty for non-FK rows
fn synth_info_key_column_usage(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("referenced_table_name", DataType::Text, false),
        ColumnSchema::new("referenced_column_name", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // FKs.
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            for (i, (&local, &parent)) in fk
                .local_columns
                .iter()
                .zip(fk.parent_columns.iter())
                .enumerate()
            {
                let parent_name = cat
                    .get(&fk.parent_table)
                    .and_then(|pt| pt.schema().columns.get(parent).map(|c| c.name.clone()))
                    .unwrap_or_else(|| alloc::format!("col{parent}"));
                #[allow(clippy::cast_possible_wrap)]
                let ordinal = (i + 1) as i32;
                rows.push(Row::new(alloc::vec![
                    Value::Text(conname.clone()),
                    Value::Text(tname.clone()),
                    Value::Text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::Text(fk.parent_table.clone()),
                    Value::Text(parent_name),
                ]));
            }
        }
        // PK / composite UC entries.
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let conname = if uc.is_primary_key {
                alloc::format!("{}_pkey", tname)
            } else {
                alloc::format!("{}_uniq{ci}", tname)
            };
            for (i, &local) in uc.columns.iter().enumerate() {
                #[allow(clippy::cast_possible_wrap)]
                let ordinal = (i + 1) as i32;
                rows.push(Row::new(alloc::vec![
                    Value::Text(conname.clone()),
                    Value::Text(tname.clone()),
                    Value::Text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::Text(String::new()),
                    Value::Text(String::new()),
                ]));
            }
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise
/// `information_schema.REFERENTIAL_CONSTRAINTS`. One row per FK.
fn synth_info_referential_constraints(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("referenced_table_name", DataType::Text, false),
        ColumnSchema::new("update_rule", DataType::Text, false),
        ColumnSchema::new("delete_rule", DataType::Text, false),
    ];
    fn rule_name(a: spg_storage::FkAction) -> &'static str {
        match a {
            spg_storage::FkAction::Cascade => "CASCADE",
            spg_storage::FkAction::SetNull => "SET NULL",
            spg_storage::FkAction::SetDefault => "SET DEFAULT",
            spg_storage::FkAction::Restrict => "RESTRICT",
            spg_storage::FkAction::NoAction => "NO ACTION",
        }
    }
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            rows.push(Row::new(alloc::vec![
                Value::Text(conname),
                Value::Text(tname.clone()),
                Value::Text(fk.parent_table.clone()),
                Value::Text(rule_name(fk.on_update).into()),
                Value::Text(rule_name(fk.on_delete).into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise `information_schema.STATISTICS`.
/// One row per (index × column) — admin tools walk this to
/// surface index-cardinality estimates.
fn synth_info_statistics(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("index_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("seq_in_index", DataType::Int, false),
        ColumnSchema::new("non_unique", DataType::Int, false),
        ColumnSchema::new("index_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            let col = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            rows.push(Row::new(alloc::vec![
                Value::Text(tname.clone()),
                Value::Text(idx.name.clone()),
                Value::Text(col),
                Value::Int(1),
                Value::Int(i32::from(!idx.is_unique)),
                Value::Text("BTREE".into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise `information_schema.ROUTINES`.
/// SPG has no user-defined functions in v7.17 so the surface is
/// always empty; admin tools just need the table to exist.
fn synth_info_routines() -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("routine_name", DataType::Text, false),
        ColumnSchema::new("routine_type", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
    ];
    (schema, Vec::new())
}

/// v7.17.0 Phase 3.P0-54 — synthesise `pg_catalog.pg_constraint`.
/// ORM compilers (Diesel, sea-orm) and admin tools probe this for
/// FK / UNIQUE / PK / CHECK definitions to surface relationship
/// graphs and validation rules. SPG ships one row per
/// uniqueness constraint + foreign key declared in the catalog.
///
/// Schema columns exposed:
///   * conname (Text) — constraint name (synthetic when anonymous)
///   * contype (Text) — `p` PK, `u` UNIQUE, `f` FK, `c` CHECK
///   * conrelid (Text) — owner table name
///   * confrelid (Text) — referenced parent table (FK only;
///     empty string otherwise)
///   * conkey (Text) — comma-separated column names
///   * confkey (Text) — comma-separated parent column names (FK only)
fn synth_pg_constraint(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("conname", DataType::Text, false),
        ColumnSchema::new("contype", DataType::Text, false),
        ColumnSchema::new("conrelid", DataType::Text, false),
        ColumnSchema::new("confrelid", DataType::Text, false),
        ColumnSchema::new("conkey", DataType::Text, false),
        ColumnSchema::new("confkey", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // Uniqueness constraints (composite UNIQUE / PRIMARY KEY).
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conname = if uc.is_primary_key {
                alloc::format!("{}_pkey", tname)
            } else {
                alloc::format!("{}_uniq{ci}", tname)
            };
            let conkey: Vec<String> = uc.columns.iter().map(|&p| col_name_at(p)).collect();
            rows.push(Row::new(alloc::vec![
                Value::Text(conname),
                Value::Text(kind.into()),
                Value::Text(tname.clone()),
                Value::Text(String::new()),
                Value::Text(conkey.join(",")),
                Value::Text(String::new()),
            ]));
        }
        // Single-column PK / UNIQUE indexes that have no
        // matching entry in `uniqueness_constraints` (the engine
        // creates only the BTree index for the bare-column case;
        // composite forms ride the UC path above).
        for idx in t.indices() {
            if !idx.is_unique {
                continue;
            }
            let is_primary = idx.name.ends_with("_pkey");
            let conname = idx.name.clone();
            let kind = if is_primary { "p" } else { "u" };
            let col_name = col_name_at(idx.column_position);
            // Skip if already emitted via the UC loop above (same
            // tuple shape — single-column).
            let already = t
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|uc| uc.columns.len() == 1 && uc.columns[0] == idx.column_position);
            if already {
                continue;
            }
            rows.push(Row::new(alloc::vec![
                Value::Text(conname),
                Value::Text(kind.into()),
                Value::Text(tname.clone()),
                Value::Text(String::new()),
                Value::Text(col_name),
                Value::Text(String::new()),
            ]));
        }
        // Foreign keys.
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            let conkey: Vec<String> = fk.local_columns.iter().map(|&p| col_name_at(p)).collect();
            // Parent column names: look up the parent table's
            // schema if it exists; otherwise emit positions.
            let confkey: Vec<String> = if let Some(parent) = cat.get(&fk.parent_table) {
                fk.parent_columns
                    .iter()
                    .map(|&p| {
                        parent
                            .schema()
                            .columns
                            .get(p)
                            .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
                    })
                    .collect()
            } else {
                fk.parent_columns
                    .iter()
                    .map(|p| alloc::format!("col{p}"))
                    .collect()
            };
            rows.push(Row::new(alloc::vec![
                Value::Text(conname),
                Value::Text("f".into()),
                Value::Text(tname.clone()),
                Value::Text(fk.parent_table.clone()),
                Value::Text(conkey.join(",")),
                Value::Text(confkey.join(",")),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_database`.
/// SPG is single-database so we surface a single row keyed on the
/// canonical `postgres` database name (matching what every PG
/// admin tool's startup screen expects to find).
fn synth_pg_database(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("datdba", DataType::BigInt, false),
        ColumnSchema::new("encoding", DataType::Int, false),
        ColumnSchema::new("datcollate", DataType::Text, false),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        Value::Text("postgres".into()),
        Value::BigInt(10),
        Value::Int(6), // UTF8
        Value::Text("en_US.UTF-8".into()),
    ])];
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_roles`. PG's
/// pg_roles is a view over pg_authid showing all roles. SPG ships
/// one row per declared user from the engine's UserStore so admin
/// tool startup screens can populate.
fn synth_pg_roles(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("rolname", DataType::Text, false),
        ColumnSchema::new("rolsuper", DataType::Bool, false),
        ColumnSchema::new("rolinherit", DataType::Bool, false),
        ColumnSchema::new("rolcanlogin", DataType::Bool, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    let oid: i64 = 10;
    for (i, (name, _)) in engine.users.iter().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid + (i as i64) + 1),
            Value::Text(name.to_string()),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
        ]));
    }
    // Always include `postgres` as the bootstrap superuser if not
    // already present — admin tools probe for it.
    if !rows
        .iter()
        .any(|r| matches!(&r.values[1], Value::Text(s) if s == "postgres"))
    {
        rows.insert(
            0,
            Row::new(alloc::vec![
                Value::BigInt(10),
                Value::Text("postgres".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ]),
        );
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-56 — synthesise `pg_catalog.pg_views`. PG's
/// pg_views is a view listing every catalog view; SPG ships one
/// row per declared view + its definition text.
/// Synthesise `pg_catalog.pg_extension`. SPG ships its "extension"
/// surfaces natively (vector, pg_trgm, plpgsql-shaped DO blocks), so
/// the table lists those as installed — `SELECT … FROM pg_extension
/// WHERE extname = 'vector'` probes from PG clients (mailrs embed
/// round-12) answer truthfully about capability presence.
fn synth_pg_extension() -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("extname", DataType::Text, false),
        ColumnSchema::new("extversion", DataType::Text, false),
        ColumnSchema::new("extnamespace", DataType::Text, false),
    ];
    let exts: &[(&str, &str)] = &[("plpgsql", "1.0"), ("vector", "0.8.0"), ("pg_trgm", "1.6")];
    let rows = exts
        .iter()
        .enumerate()
        .map(|(i, (name, ver))| {
            Row::new(alloc::vec![
                Value::BigInt(16384 + i as i64),
                Value::Text((*name).into()),
                Value::Text((*ver).into()),
                Value::Text("pg_catalog".into()),
            ])
        })
        .collect();
    (schema, rows)
}

fn synth_pg_views(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("viewname", DataType::Text, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for (name, def) in cat.views() {
        rows.push(Row::new(alloc::vec![
            Value::Text("public".into()),
            Value::Text(name.clone()),
            Value::Text(def.body.clone()),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-57 — synthesise `pg_catalog.pg_settings`. ORM
/// connection-checkers (sqlx pre-flight, Diesel migrator) and admin
/// tools read `pg_settings` to discover server-side configuration.
/// SPG surfaces every session_param + a small set of canonical PG
/// defaults so the pre-flight queries match.
fn synth_pg_settings(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("setting", DataType::Text, false),
        ColumnSchema::new("category", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    // Canonical defaults every admin tool expects to find.
    let defaults: &[(&str, &str, &str)] = &[
        ("server_version", "16.0 (spg)", "Preset Options"),
        ("server_encoding", "UTF8", "Client Connection Defaults"),
        ("client_encoding", "UTF8", "Client Connection Defaults"),
        ("DateStyle", "ISO, MDY", "Client Connection Defaults"),
        ("TimeZone", "UTC", "Client Connection Defaults"),
        ("standard_conforming_strings", "on", "Compatibility"),
        ("integer_datetimes", "on", "Compatibility"),
        ("max_connections", "100", "Connections and Authentication"),
    ];
    for &(name, val, cat) in defaults {
        rows.push(Row::new(alloc::vec![
            Value::Text(name.into()),
            Value::Text(val.into()),
            Value::Text(cat.into()),
        ]));
    }
    // Session-set params override the static defaults.
    for (k, v) in &engine.session_params {
        if !defaults
            .iter()
            .any(|(n, _, _)| (*n).eq_ignore_ascii_case(k))
        {
            rows.push(Row::new(alloc::vec![
                Value::Text(k.clone()),
                Value::Text(v.clone()),
                Value::Text("Session".into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-53 — synthesise `pg_catalog.pg_indexes`.
/// PG's pg_indexes is a real view on pg_index + pg_class + pg_attribute.
/// SPG ships it as a synthesised flat table so admin tools (pgAdmin,
/// DataGrip) can list indexes by tablename without joining four catalogs.
///
/// Schema columns exposed:
///   * schemaname (Text) — always `public`
///   * tablename (Text)
///   * indexname (Text)
///   * indexdef (Text) — best-effort CREATE INDEX DDL
fn synth_pg_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("indexname", DataType::Text, false),
        ColumnSchema::new("indexdef", DataType::Text, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            let col_name = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            let unique_kw = if idx.is_unique { "UNIQUE " } else { "" };
            let indexdef = alloc::format!(
                "CREATE {unique_kw}INDEX {} ON public.{} ({})",
                idx.name,
                tname,
                col_name
            );
            rows.push(Row::new(alloc::vec![
                Value::Text("public".into()),
                Value::Text(tname.clone()),
                Value::Text(idx.name.clone()),
                Value::Text(indexdef),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-53 — synthesise `pg_catalog.pg_index`. The
/// "raw" pg_index catalog used by PG-internal tooling for index
/// flags and ordinal information. SPG ships the columns ORM probes
/// actually filter on.
///
/// Schema columns exposed:
///   * indexrelid (BigInt) — index OID (synthetic = position+1)
///   * indrelid (BigInt) — table OID (synthetic = position+1)
///   * indnatts (Int) — number of indexed columns
///   * indisunique (Bool)
///   * indisprimary (Bool)
fn synth_pg_index_raw(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("indexrelid", DataType::BigInt, false),
        ColumnSchema::new("indrelid", DataType::BigInt, false),
        ColumnSchema::new("indnatts", DataType::Int, false),
        ColumnSchema::new("indisunique", DataType::Bool, false),
        ColumnSchema::new("indisprimary", DataType::Bool, false),
    ];
    let mut rows: Vec<Row> = Vec::new();
    let mut idx_oid: i64 = 100_000;
    for (table_idx, tname) in cat.table_names().iter().enumerate() {
        let Some(t) = cat.get(tname) else { continue };
        for idx in t.indices() {
            idx_oid += 1;
            #[allow(clippy::cast_possible_wrap)]
            let nattrs = (1 + idx.extra_column_positions.len()) as i32;
            // is_primary: SPG / PG flag the primary via the
            // index name convention `<table>_pkey`.
            let is_primary = idx.name.ends_with("_pkey");
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::BigInt((table_idx + 1) as i64),
                Value::Int(nattrs),
                Value::Bool(idx.is_unique),
                Value::Bool(is_primary),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-52 — synthesise `pg_catalog.pg_namespace`.
/// SPG is single-schema so we expose the canonical PG schemas:
/// `public` (user-facing), `pg_catalog` (built-in), and
/// `information_schema` (PG meta).
fn synth_pg_namespace(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("nspname", DataType::Text, false),
        ColumnSchema::new("nspowner", DataType::BigInt, false),
    ];
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(11),
            Value::Text("pg_catalog".into()),
            Value::BigInt(10),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2200),
            Value::Text("public".into()),
            Value::BigInt(10),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13000),
            Value::Text("information_schema".into()),
            Value::BigInt(10),
        ]),
    ];
    (schema, rows)
}

/// v7.16.2 — drop the synthesised meta view into the enriched
/// catalog so the regular FROM-resolution path can see it.
fn materialise_meta_view(
    catalog: &mut Catalog,
    name: &str,
    columns: Vec<ColumnSchema>,
    rows: Vec<Row>,
) -> Result<(), EngineError> {
    let schema = TableSchema::new(name.to_string(), columns);
    catalog.create_table(schema).map_err(EngineError::Storage)?;
    let table = catalog
        .get_mut(name)
        .expect("just-created meta view must exist");
    for row in rows {
        table.insert(row).map_err(EngineError::Storage)?;
    }
    Ok(())
}

/// v7.16.2 — true when the SELECT statement references any
/// `__spg_info_*` or `__spg_pg_*` synthetic table name (the
/// parser produces these for `information_schema.X` /
/// `pg_catalog.X`). Used by `exec_select_cancel` to short-
/// circuit into the meta-view materialisation path.
/// v7.17.0 Phase 1.2 — append the names of any catalog-known
/// views referenced by `tref` to `into`. Helper for
/// `Engine::expand_views_in_select`. A view that's been already
/// materialised as a table (e.g. via the synthetic CTE pass for
/// SELECT FROM v) is skipped — the table form wins so the
/// recursive exec_select_cancel call inside exec_with_ctes
/// doesn't re-expand and trigger the CTE-shadow guard.
fn collect_view_refs(
    tref: &spg_sql::ast::TableRef,
    cat: &spg_storage::Catalog,
    into: &mut Vec<String>,
) {
    if cat.views().contains_key(&tref.name)
        && cat.get(&tref.name).is_none()
        && !into.iter().any(|n| n == &tref.name)
    {
        into.push(tref.name.clone());
    }
}

fn select_references_meta_view(stmt: &SelectStatement) -> bool {
    fn is_meta(name: &str) -> bool {
        name.starts_with("__spg_info_")
            || name.starts_with("__spg_pg_")
            || name.starts_with("__spg_mysql_")
    }
    if let Some(from) = &stmt.from {
        if is_meta(&from.primary.name) {
            return true;
        }
        for j in &from.joins {
            if is_meta(&j.table.name) {
                return true;
            }
        }
    }
    for cte in &stmt.ctes {
        if select_references_meta_view(&cte.body) {
            return true;
        }
    }
    false
}

/// v7.16.2 — collect every meta-view name a SELECT touches.
/// Returns a deduplicated, sorted list. Caller materialises
/// each one into the enriched catalog before re-running the
/// SELECT. Walks JOINs, CTEs, and the primary FROM.
fn collect_meta_view_names(
    stmt: &SelectStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    fn is_meta(name: &str) -> bool {
        name.starts_with("__spg_info_")
            || name.starts_with("__spg_pg_")
            || name.starts_with("__spg_mysql_")
    }
    if let Some(from) = &stmt.from {
        if is_meta(&from.primary.name) {
            into.insert(from.primary.name.clone());
        }
        for j in &from.joins {
            if is_meta(&j.table.name) {
                into.insert(j.table.name.clone());
            }
        }
    }
    for cte in &stmt.ctes {
        collect_meta_view_names(&cte.body, into);
    }
}

fn infer_column_types(columns: &[ColumnSchema], rows: &[Row]) -> Vec<ColumnSchema> {
    let mut out = columns.to_vec();
    for (col_idx, col) in out.iter_mut().enumerate() {
        if col.ty != DataType::Text {
            continue;
        }
        let mut inferred: Option<DataType> = None;
        let mut all_null = true;
        for row in rows {
            let Some(v) = row.values.get(col_idx) else {
                continue;
            };
            let ty = match v {
                Value::Null => continue,
                Value::SmallInt(_) => DataType::SmallInt,
                Value::Int(_) => DataType::Int,
                Value::BigInt(_) => DataType::BigInt,
                Value::Float(_) => DataType::Float,
                Value::Bool(_) => DataType::Bool,
                Value::Vector(_) => DataType::Vector {
                    dim: 0,
                    encoding: VecEncoding::F32,
                },
                _ => DataType::Text,
            };
            all_null = false;
            inferred = Some(match inferred {
                None => ty,
                Some(prev) if prev == ty => prev,
                Some(_) => DataType::Text,
            });
        }
        if let Some(t) = inferred {
            col.ty = t;
            col.nullable = true;
        } else if all_null {
            col.nullable = true;
        }
    }
    out
}

/// v4.26: render a human-readable plan tree for `EXPLAIN <select>`.
/// Lines are pushed into `out`; `depth` controls indentation. We
/// describe the rewritten SELECT — what the executor *would* do —
/// using the engine handle to spot indexed lookups and table shapes.
#[allow(clippy::too_many_lines, clippy::format_push_string)]
/// v6.2.4 — Walk every line of the rendered plan tree and append
/// per-operator stats. Lines that name a known operator get
/// `(rows=N)` (`actual_rows` of the top-level operator equals the
/// final result row count; scans report their catalog row count
/// as the rows-considered metric). Other lines — Filter / Join /
/// GroupBy / OrderBy etc. — are marked `(—)` so the surface is
/// complete-by-construction; v6.2.5 fills these in via inline
/// executor counters.
/// v6.8.3 — surface "CREATE INDEX …" suggestions for every
/// `(table, column)` pair the query touches via WHERE / JOIN
/// that doesn't already have an index on the owning table.
/// Walks the SELECT's FROM clauses + WHERE expression tree;
/// returns one line per missing index. Deterministic order:
/// FROM-clause iteration order, then column-reference walk
/// order inside each WHERE. Each suggestion is a copy-pastable
/// DDL string.
fn build_index_suggestions(stmt: &SelectStatement, engine: &Engine) -> Vec<String> {
    use alloc::collections::BTreeSet;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let cat = engine.active_catalog();
    // Build a (table, qualifier-or-alias) list from the FROM clause
    // so unqualified column refs in WHERE resolve to the correct
    // table.
    let Some(from) = &stmt.from else {
        return out;
    };
    let mut tables: Vec<String> = Vec::new();
    tables.push(from.primary.name.clone());
    for j in &from.joins {
        tables.push(j.table.name.clone());
    }
    // Collect column refs from the WHERE expression. JOIN ON
    // predicates also feed in.
    let mut col_refs: Vec<spg_sql::ast::ColumnName> = Vec::new();
    if let Some(w) = &stmt.where_ {
        collect_column_refs(w, &mut col_refs);
    }
    for j in &from.joins {
        if let Some(on) = &j.on {
            collect_column_refs(on, &mut col_refs);
        }
    }
    for cn in &col_refs {
        // Resolve owner table: explicit qualifier first, else
        // first table in FROM that has a column of this name.
        let owner: Option<String> = if let Some(q) = &cn.qualifier {
            tables.iter().find(|t| t == &q).cloned()
        } else {
            tables.iter().find_map(|t| {
                cat.get(t).and_then(|tbl| {
                    if tbl.schema().column_position(&cn.name).is_some() {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
            })
        };
        let Some(owner) = owner else {
            continue;
        };
        let Some(tbl) = cat.get(&owner) else {
            continue;
        };
        let Some(col_pos) = tbl.schema().column_position(&cn.name) else {
            continue;
        };
        // Skip if any BTree index already covers this column as
        // its key.
        let already_indexed = tbl.indices().iter().any(|i| {
            matches!(i.kind, spg_storage::IndexKind::BTree(_))
                && i.column_position == col_pos
                && i.expression.is_none()
                && i.partial_predicate.is_none()
        });
        if already_indexed {
            continue;
        }
        if seen.insert((owner.clone(), cn.name.clone())) {
            out.push(alloc::format!(
                "SUGGEST: CREATE INDEX ix_{}_{} ON {} ({})",
                owner,
                cn.name,
                owner,
                cn.name
            ));
        }
    }
    out
}

/// Walks an `Expr` and pushes every `ColumnName` it references.
/// Order is depth-first, left-to-right.
fn collect_column_refs(expr: &Expr, out: &mut Vec<spg_sql::ast::ColumnName>) {
    match expr {
        Expr::Column(cn) => out.push(cn.clone()),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_column_refs(a, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_column_refs(lhs, out);
            collect_column_refs(rhs, out);
        }
        Expr::Unary { expr: e, .. } => collect_column_refs(e, out),
        _ => {}
    }
}

fn annotate_explain_lines(lines: &mut [String], total_rows: usize, engine: &Engine) {
    let catalog = engine.active_catalog();
    let cold_ids = catalog.cold_segment_ids_global();
    let any_cold = !cold_ids.is_empty();
    let cold_ids_repr = if any_cold {
        let mut s = alloc::string::String::from("[");
        for (i, id) in cold_ids.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&alloc::format!("{id}"));
        }
        s.push(']');
        s
    } else {
        alloc::string::String::new()
    };
    for (idx, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim_start();
        let is_top_level = idx == 0;
        if is_top_level {
            line.push_str(&alloc::format!(" (rows={total_rows})"));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("From: ") {
            let (name, scan_kind) = match rest.split_once(" [") {
                Some((n, k)) => (n.trim(), k.trim_end_matches(']')),
                None => (rest.trim(), ""),
            };
            let bare = name.split_whitespace().next().unwrap_or(name);
            let hot = catalog.get(bare).map(|t| t.rows().len());
            // v6.2.7 — `cold_segments=[id0,id1,…]` enumerates every
            // cold-tier segment the scan COULD have walked. v6.2.x
            // can tighten to per-table by walking the table's
            // BTree-index cold locators.
            let annot = match (hot, scan_kind) {
                (Some(h), "full scan") => {
                    let mut s = alloc::format!(" (hot_rows={h}");
                    if any_cold {
                        s.push_str(&alloc::format!(
                            ", cold_tier=present, cold_segments={cold_ids_repr}"
                        ));
                    }
                    s.push(')');
                    s
                }
                (Some(h), "index seek") => {
                    let mut s = alloc::format!(" (hot_rows≤{h}");
                    if any_cold {
                        s.push_str(&alloc::format!(
                            ", cold_tier=present, cold_segments={cold_ids_repr}"
                        ));
                    }
                    s.push(')');
                    s
                }
                _ => " (rows=—)".to_string(),
            };
            line.push_str(&annot);
            continue;
        }
        // Filter / GroupBy / Having / OrderBy / Limit / Join etc.
        line.push_str(" (rows=—)");
    }
}

fn explain_select(stmt: &SelectStatement, engine: &Engine, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    // 1) Top-level operator label.
    let top = if !stmt.ctes.is_empty() {
        if stmt.ctes.iter().any(|c| c.recursive) {
            "CTEScan (WITH RECURSIVE)"
        } else {
            "CTEScan (WITH)"
        }
    } else if !stmt.unions.is_empty() {
        "UnionScan"
    } else if select_has_window(stmt) {
        "WindowAgg"
    } else if aggregate::uses_aggregate(stmt) {
        "Aggregate"
    } else if stmt.distinct {
        "Distinct"
    } else if stmt.from.is_some() {
        "TableScan"
    } else {
        "Result"
    };
    out.push(alloc::format!("{pad}{top}"));
    let child = "  ".repeat(depth + 1);
    // 2) CTE bodies.
    for cte in &stmt.ctes {
        let head = if cte.recursive {
            alloc::format!("{child}CTE (recursive): {}", cte.name)
        } else {
            alloc::format!("{child}CTE: {}", cte.name)
        };
        out.push(head);
        explain_select(&cte.body, engine, depth + 2, out);
    }
    // 3) FROM details — primary table + joins, index hits.
    if let Some(from) = &stmt.from {
        let mut tag = alloc::format!("{child}From: {}", from.primary.name);
        if let Some(alias) = &from.primary.alias {
            tag.push_str(&alloc::format!(" AS {alias}"));
        }
        // Try to detect an index-seek opportunity on WHERE against
        // the primary table — same heuristic the executor uses.
        if let Some(w) = &stmt.where_
            && let Some(table) = engine.active_catalog().get(&from.primary.name)
        {
            let alias = from.primary.alias.as_deref().unwrap_or(&from.primary.name);
            let cols = &table.schema().columns;
            if try_index_seek(w, cols, engine.active_catalog(), table, alias).is_some() {
                tag.push_str(" [index seek]");
            } else {
                tag.push_str(" [full scan]");
            }
        } else {
            tag.push_str(" [full scan]");
        }
        out.push(tag);
        for j in &from.joins {
            let kind = match j.kind {
                spg_sql::ast::JoinKind::Inner => "INNER JOIN",
                spg_sql::ast::JoinKind::Left => "LEFT JOIN",
                spg_sql::ast::JoinKind::Cross => "CROSS JOIN",
            };
            let mut s = alloc::format!("{child}{kind}: {}", j.table.name);
            if let Some(alias) = &j.table.alias {
                s.push_str(&alloc::format!(" AS {alias}"));
            }
            if j.on.is_some() {
                s.push_str(" (ON …)");
            }
            out.push(s);
        }
    }
    // 4) WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET.
    if let Some(w) = &stmt.where_ {
        let mut s = alloc::format!("{child}Filter: {w}");
        if expr_has_subquery(w) {
            s.push_str(" [subquery]");
        }
        out.push(s);
    }
    if let Some(gs) = &stmt.group_by {
        let mut parts = Vec::new();
        for g in gs {
            parts.push(alloc::format!("{g}"));
        }
        out.push(alloc::format!("{child}GroupBy: {}", parts.join(", ")));
    }
    if let Some(h) = &stmt.having {
        out.push(alloc::format!("{child}Having: {h}"));
    }
    for o in &stmt.order_by {
        let dir = if o.desc { "DESC" } else { "ASC" };
        out.push(alloc::format!("{child}OrderBy: {} {dir}", o.expr));
    }
    if let Some(lim) = stmt.limit {
        out.push(alloc::format!("{child}Limit: {lim}"));
    }
    if let Some(off) = stmt.offset {
        out.push(alloc::format!("{child}Offset: {off}"));
    }
    // 5) Projection — collapse Wildcard or render N items.
    if stmt
        .items
        .iter()
        .any(|it| matches!(it, SelectItem::Wildcard))
    {
        out.push(alloc::format!("{child}Project: *"));
    } else {
        out.push(alloc::format!(
            "{child}Project: {} item(s)",
            stmt.items.len()
        ));
    }
    // 6) Recurse into UNION peers.
    for (kind, peer) in &stmt.unions {
        let label = match kind {
            UnionKind::All => "UNION ALL",
            UnionKind::Distinct => "UNION",
        };
        out.push(alloc::format!("{child}{label}"));
        explain_select(peer, engine, depth + 2, out);
    }
}

/// v4.23: recognise the engine errors that indicate the inner
/// SELECT couldn't be evaluated in isolation because it references
/// an outer column — used by `subquery_replacement` to skip
/// materialisation and let row-eval handle it instead.
fn is_correlation_error(e: &EngineError) -> bool {
    matches!(
        e,
        EngineError::Eval(
            eval::EvalError::ColumnNotFound { .. } | eval::EvalError::UnknownQualifier { .. }
        )
    )
}

/// v4.23: walk every Expr in `stmt` and replace each Column ref
/// that targets the outer scope (qualifier matches the outer
/// table alias) with a Literal carrying the outer row's value.
/// Conservative: only qualified refs are substituted, so the user
/// must write `outer_alias.col` to reference an outer column. This
/// matches PG's lexical scoping for correlated subqueries and
/// avoids accidentally rebinding inner columns of the same name.
/// v7.17.0 Phase 3.P0-41 — LATERAL peer descriptor. Either eagerly
/// materialised (every regular table / unnest / generate_series) or
/// lateral (subquery re-evaluated per outer row).
struct JoinedPeer<'a> {
    eager_rows: Option<Vec<Row>>,
    cols: Vec<ColumnSchema>,
    alias: String,
    kind: JoinKind,
    on: Option<&'a Expr>,
    lateral: Option<&'a SelectStatement>,
    /// v7.28 (round-22) — plain-table name for the index-nested-loop
    /// path. None for unnest/lateral.
    join_table: Option<String>,
}

/// v7.17.0 Phase 3.P0-41 — synthesise a column name for a LATERAL
/// projection item that has no explicit alias. PG names anonymous
/// projection items by the function call's name or by `column<i>`.
/// SPG mirrors the latter (lower-overhead than walking arbitrary
/// Expr shapes) so the probe-schema fallback path produces stable
/// names for the lateral peer's columns.
fn synth_lateral_col_name(expr: &Expr, idx: usize) -> String {
    match expr {
        // Bare column reference — use the column's own name.
        Expr::Column(c) => c.name.clone(),
        // Function call — use the function name (PG canonical:
        // `count` / `max` / `lower` …).
        Expr::FunctionCall { name, .. } => name.clone(),
        // Cast — drill into the inner expression.
        Expr::Cast { expr: inner, .. } => synth_lateral_col_name(inner, idx),
        // Everything else falls back to PG's `column<N>` placeholder.
        _ => alloc::format!("column{}", idx + 1),
    }
}

/// v7.17.0 Phase 3.P0-41 — substitute every `<alias>.<col>` Expr
/// reference whose `<alias>.<col>` exists in the outer composite
/// schema with the matching value from the outer row. Walks the
/// entire SELECT body (items, WHERE, GROUP BY, HAVING, ORDER BY,
/// UNION peers) so any depth of outer reference inside the
/// LATERAL subquery resolves before execution.
fn substitute_outer_columns_multi(
    stmt: &mut SelectStatement,
    outer_row: &Row,
    outer_schema: &[ColumnSchema],
) {
    substitute_outer_in_select(stmt, outer_row, outer_schema);
}

fn substitute_outer_in_select(
    stmt: &mut SelectStatement,
    outer_row: &Row,
    outer_schema: &[ColumnSchema],
) {
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_outer_in_expr(expr, outer_row, outer_schema);
        }
    }
    if let Some(w) = &mut stmt.where_ {
        substitute_outer_in_expr(w, outer_row, outer_schema);
    }
    if let Some(gs) = &mut stmt.group_by {
        for g in gs {
            substitute_outer_in_expr(g, outer_row, outer_schema);
        }
    }
    if let Some(h) = &mut stmt.having {
        substitute_outer_in_expr(h, outer_row, outer_schema);
    }
    for o in &mut stmt.order_by {
        substitute_outer_in_expr(&mut o.expr, outer_row, outer_schema);
    }
    for (_, peer) in &mut stmt.unions {
        substitute_outer_in_select(peer, outer_row, outer_schema);
    }
}

fn substitute_outer_in_expr(e: &mut Expr, outer_row: &Row, outer_schema: &[ColumnSchema]) {
    if let Expr::Column(c) = e
        && let Some(qual) = &c.qualifier
    {
        let composite = alloc::format!("{qual}.{}", c.name);
        if let Some(idx) = outer_schema
            .iter()
            .position(|sc| sc.name.eq_ignore_ascii_case(&composite))
        {
            let v = outer_row.values.get(idx).cloned().unwrap_or(Value::Null);
            if let Ok(lit) = value_to_literal_expr(v) {
                *e = lit;
                return;
            }
        }
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            substitute_outer_in_expr(lhs, outer_row, outer_schema);
            substitute_outer_in_expr(rhs, outer_row, outer_schema);
        }
        Expr::Unary { expr: inner, .. } => {
            substitute_outer_in_expr(inner, outer_row, outer_schema);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_outer_in_expr(a, outer_row, outer_schema);
            }
        }
        Expr::Cast { expr: inner, .. } => {
            substitute_outer_in_expr(inner, outer_row, outer_schema);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                substitute_outer_in_expr(op, outer_row, outer_schema);
            }
            for (cond, val) in branches {
                substitute_outer_in_expr(cond, outer_row, outer_schema);
                substitute_outer_in_expr(val, outer_row, outer_schema);
            }
            if let Some(e) = else_branch {
                substitute_outer_in_expr(e, outer_row, outer_schema);
            }
        }
        _ => {}
    }
}

impl Engine {
    /// v7.29 (round-22 phase 3) — try to batch-evaluate a correlated
    /// scalar subquery of the shape
    ///   (SELECT expr FROM … WHERE inner_preds AND inner_col = outer_col
    ///    [ORDER BY o [DESC]] [LIMIT 1])
    /// by running the subquery ONCE without the correlation and
    /// folding rows into a key→value map (group top-1 when ordered).
    /// Returns None when the shape doesn't qualify; correctness then
    /// falls back to per-row execution.
    fn try_batch_correlated_scalar(
        &self,
        inner: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Option<memoize::GroupMap>, EngineError> {
        use spg_sql::ast::{BinOp, SelectItem as SI};
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || inner.items.len() != 1
            || inner.order_by.len() > 1
            || inner.offset.is_some()
        {
            return Ok(None);
        }
        // LIMIT must be absent or literally 1 (top-1 semantics).
        if let Some(le) = inner.limit
            && le.as_literal() != Some(1)
        {
            return Ok(None);
        }
        let Some(from) = &inner.from else {
            return Ok(None);
        };
        if from.primary.lateral_subquery.is_some() || from.primary.unnest_expr.is_some() {
            return Ok(None);
        }
        // Inner alias set.
        let mut inner_aliases: Vec<String> = Vec::new();
        inner_aliases.push(
            from.primary
                .alias
                .clone()
                .unwrap_or_else(|| from.primary.name.clone()),
        );
        for j in &from.joins {
            if j.table.lateral_subquery.is_some() || j.table.unnest_expr.is_some() {
                return Ok(None);
            }
            inner_aliases.push(
                j.table
                    .alias
                    .clone()
                    .unwrap_or_else(|| j.table.name.clone()),
            );
        }
        let is_inner = |c: &spg_sql::ast::ColumnName| -> bool {
            match &c.qualifier {
                Some(q) => inner_aliases.iter().any(|a| a.eq_ignore_ascii_case(q)),
                None => false,
            }
        };
        let is_outer = |c: &spg_sql::ast::ColumnName| -> bool {
            match &c.qualifier {
                Some(q) => !inner_aliases.iter().any(|a| a.eq_ignore_ascii_case(q)),
                // Synthetic group columns arrive bare after the
                // aggregate rewrite.
                None => c.name.starts_with("__grp_") || c.name.starts_with("__agg_"),
            }
        };
        // Every expression OTHER than the correlation conjunct must be
        // fully inner (qualified to inner aliases).
        let all_inner = |e: &Expr| -> bool {
            let mut cols: Vec<spg_sql::ast::ColumnName> = Vec::new();
            let mut subs: Vec<&SelectStatement> = Vec::new();
            visit_expr_columns_and_subqueries(e, &mut |c| cols.push(c.clone()), &mut |sub| {
                subs.push(sub)
            });
            subs.is_empty() && cols.iter().all(|c| is_inner(c) && !c.name.is_empty())
        };
        let Some(w) = &inner.where_ else {
            return Ok(None);
        };
        let conjuncts = reorder::split_and_conjunctions(w);
        let mut corr: Option<(spg_sql::ast::ColumnName, spg_sql::ast::ColumnName)> = None; // (inner, outer)
        let mut rest: Vec<&Expr> = Vec::new();
        for c in conjuncts {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    if corr.is_some() {
                        return Ok(None); // more than one correlation
                    }
                    corr = Some(p);
                    continue;
                }
            }
            if !all_inner(c) {
                return Ok(None);
            }
            rest.push(c);
        }
        let Some((inner_col, outer_col)) = corr else {
            return Ok(None);
        };
        let SI::Expr { expr: out_expr, .. } = &inner.items[0] else {
            return Ok(None);
        };
        if !all_inner(out_expr) {
            return Ok(None);
        }
        let order = inner.order_by.first();
        if let Some(o) = order
            && !all_inner(&o.expr)
        {
            return Ok(None);
        }
        // Build the batch statement: SELECT inner_col, [order], expr
        // FROM … WHERE rest — no correlation, no order, no limit.
        let mut batch = inner.clone();
        batch.limit = None;
        batch.offset = None;
        batch.order_by = Vec::new();
        batch.where_ = rest
            .iter()
            .map(|e| (*e).clone())
            .reduce(|a, b| Expr::Binary {
                lhs: alloc::boxed::Box::new(a),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(b),
            });
        let mut items: Vec<SI> = alloc::vec![SI::Expr {
            expr: Expr::Column(inner_col),
            alias: None,
        }];
        if let Some(o) = order {
            items.push(SI::Expr {
                expr: o.expr.clone(),
                alias: None,
            });
        }
        items.push(SI::Expr {
            expr: out_expr.clone(),
            alias: None,
        });
        batch.items = items;
        let r = self.exec_select_cancel(&batch, cancel)?;
        let QueryResult::Rows { rows, .. } = r else {
            return Ok(None);
        };
        let has_order = order.is_some();
        let (desc, nf) = order
            .map(|o| (o.desc, o.nulls_first))
            .unwrap_or((false, None));
        let mut best: alloc::collections::BTreeMap<String, (Option<Value>, Value)> =
            alloc::collections::BTreeMap::new();
        for row in rows {
            let key_v = row.values.first().cloned().unwrap_or(Value::Null);
            if matches!(key_v, Value::Null) {
                continue;
            }
            let key = aggregate::encode_key(core::slice::from_ref(&key_v));
            let (ord_v, out_v) = if has_order {
                (
                    Some(row.values.get(1).cloned().unwrap_or(Value::Null)),
                    row.values.get(2).cloned().unwrap_or(Value::Null),
                )
            } else {
                (None, row.values.get(1).cloned().unwrap_or(Value::Null))
            };
            match best.get(&key) {
                None => {
                    best.insert(key, (ord_v, out_v));
                }
                Some((cur_ord, _)) if has_order => {
                    // The sorted-first row wins: candidate beats the
                    // incumbent when it compares LESS under the key's
                    // ordering.
                    let cand = ord_v.clone().unwrap_or(Value::Null);
                    let cur = cur_ord.clone().unwrap_or(Value::Null);
                    if order_by_value_cmp(desc, nf, &cand, &cur) == core::cmp::Ordering::Less {
                        best.insert(key, (ord_v, out_v));
                    }
                }
                Some(_) => {} // unordered: first row stands (any row is valid)
            }
        }
        let map = best.into_iter().map(|(k, (_, v))| (k, v)).collect();
        Ok(Some((outer_col, map)))
    }
}

/// v7.29 (3c) — pre-order collection of SCALAR subquery nodes in a
/// host expression (no descent into subquery bodies). The splice
/// walk below uses the same order; the pair must stay in lockstep.
fn collect_scalar_subqueries<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::ScalarSubquery(s) => out.push(s),
        Expr::Exists { .. } | Expr::InSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            collect_scalar_subqueries(lhs, out);
            collect_scalar_subqueries(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_scalar_subqueries(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_scalar_subqueries(expr, out);
            collect_scalar_subqueries(pattern, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_scalar_subqueries(a, out);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            collect_scalar_subqueries(call, out);
            for o in order_by {
                collect_scalar_subqueries(&o.expr, out);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                collect_scalar_subqueries(op, out);
            }
            for (w, t) in branches {
                collect_scalar_subqueries(w, out);
                collect_scalar_subqueries(t, out);
            }
            if let Some(eb) = else_branch {
                collect_scalar_subqueries(eb, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            collect_scalar_subqueries(target, out);
            collect_scalar_subqueries(index, out);
        }
        _ => {}
    }
}

/// v7.29 (3d) — empty every scalar-subquery BODY in a host
/// expression (node kept so the splice pre-order still matches).
fn hollow_scalar_subqueries(e: &mut Expr) {
    match e {
        Expr::ScalarSubquery(s) => {
            let hollow = SelectStatement {
                items: Vec::new(),
                ..SelectStatement::default()
            };
            **s = hollow;
        }
        Expr::Exists { .. } | Expr::InSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            hollow_scalar_subqueries(lhs);
            hollow_scalar_subqueries(rhs);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            hollow_scalar_subqueries(expr);
        }
        Expr::Like { expr, pattern, .. } => {
            hollow_scalar_subqueries(expr);
            hollow_scalar_subqueries(pattern);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                hollow_scalar_subqueries(a);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            hollow_scalar_subqueries(call);
            for o in order_by.iter_mut() {
                hollow_scalar_subqueries(&mut o.expr);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                hollow_scalar_subqueries(op);
            }
            for (w, t) in branches.iter_mut() {
                hollow_scalar_subqueries(w);
                hollow_scalar_subqueries(t);
            }
            if let Some(eb) = else_branch {
                hollow_scalar_subqueries(eb);
            }
        }
        Expr::ArraySubscript { target, index } => {
            hollow_scalar_subqueries(target);
            hollow_scalar_subqueries(index);
        }
        _ => {}
    }
}

/// v7.29 (3c) — splice the i-th scalar subquery's batched value into
/// the cloned tree (same pre-order as collect_scalar_subqueries).
/// Returns Ok(false) if a literal conversion fails (caller falls
/// back to the resolver path).
fn splice_planned_subqueries(
    e: &mut Expr,
    plan: &[Option<alloc::rc::Rc<memoize::GroupMap>>],
    idx: &mut usize,
    row: &Row,
    ctx: &EvalContext<'_>,
) -> Result<bool, EngineError> {
    match e {
        Expr::ScalarSubquery(_) => {
            let Some(Some(gm)) = plan.get(*idx) else {
                return Ok(false);
            };
            *idx += 1;
            let (outer_col, map) = gm.as_ref();
            let key_v = eval::eval_expr(&Expr::Column(outer_col.clone()), row, ctx)
                .map_err(EngineError::Eval)?;
            let v = if matches!(key_v, Value::Null) {
                Value::Null
            } else {
                map.get(&aggregate::encode_key(core::slice::from_ref(&key_v)))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            *e = value_to_literal_expr(v)?;
            Ok(true)
        }
        Expr::Exists { .. } | Expr::InSubquery { .. } => Ok(true),
        Expr::Binary { lhs, rhs, .. } => Ok(splice_planned_subqueries(lhs, plan, idx, row, ctx)?
            && splice_planned_subqueries(rhs, plan, idx, row, ctx)?),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            splice_planned_subqueries(expr, plan, idx, row, ctx)
        }
        Expr::Like { expr, pattern, .. } => {
            Ok(splice_planned_subqueries(expr, plan, idx, row, ctx)?
                && splice_planned_subqueries(pattern, plan, idx, row, ctx)?)
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                if !splice_planned_subqueries(a, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            if !splice_planned_subqueries(call, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for o in order_by.iter_mut() {
                if !splice_planned_subqueries(&mut o.expr, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                if !splice_planned_subqueries(op, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            for (w, t) in branches.iter_mut() {
                if !splice_planned_subqueries(w, plan, idx, row, ctx)?
                    || !splice_planned_subqueries(t, plan, idx, row, ctx)?
                {
                    return Ok(false);
                }
            }
            if let Some(eb) = else_branch {
                if !splice_planned_subqueries(eb, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::ArraySubscript { target, index } => {
            Ok(splice_planned_subqueries(target, plan, idx, row, ctx)?
                && splice_planned_subqueries(index, plan, idx, row, ctx)?)
        }
        _ => Ok(true),
    }
}

fn substitute_outer_columns(stmt: &mut SelectStatement, row: &Row, ctx: &EvalContext<'_>) {
    // v7.24 (round-16 B) — joined outer contexts carry no single
    // table alias; their schemas use composite "alias.column" names
    // instead. Pass an unmatchable alias and let the composite
    // lookup in substitute_in_expr do the work (a correlated EXISTS
    // under a JOIN previously skipped substitution entirely and
    // died with "unknown table qualifier").
    let outer_alias = ctx.table_alias.unwrap_or("");
    substitute_in_select(stmt, row, ctx, outer_alias);
}

fn substitute_in_select(
    stmt: &mut SelectStatement,
    row: &Row,
    ctx: &EvalContext<'_>,
    outer_alias: &str,
) {
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
    }
    if let Some(w) = &mut stmt.where_ {
        substitute_in_expr(w, row, ctx, outer_alias);
    }
    if let Some(gs) = &mut stmt.group_by {
        for g in gs {
            substitute_in_expr(g, row, ctx, outer_alias);
        }
    }
    if let Some(h) = &mut stmt.having {
        substitute_in_expr(h, row, ctx, outer_alias);
    }
    for o in &mut stmt.order_by {
        substitute_in_expr(&mut o.expr, row, ctx, outer_alias);
    }
    for (_, peer) in &mut stmt.unions {
        substitute_in_select(peer, row, ctx, outer_alias);
    }
}

fn substitute_in_expr(e: &mut Expr, row: &Row, ctx: &EvalContext<'_>, outer_alias: &str) {
    // v7.25.2 (round-19 A) — bare synthetic columns. The aggregate
    // rewriter replaces group-key references INSIDE subquery bodies
    // with `__grp_N` so a correlated subquery in a GROUP BY select
    // list can resolve against the synthesised group row. The names
    // are engine-generated, so they can't shadow user columns.
    if let Expr::Column(c) = e
        && c.qualifier.is_none()
        && (c.name.starts_with("__grp_") || c.name.starts_with("__agg_"))
        && let Some(idx) = ctx.columns.iter().position(|sc| sc.name == c.name)
    {
        let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
        if let Ok(lit) = value_to_literal_expr(v) {
            *e = lit;
            return;
        }
    }
    if let Expr::Column(c) = e
        && let Some(qual) = &c.qualifier
    {
        // Look up the column's index in the outer schema: plain name
        // when the qualifier is the outer table's alias, composite
        // "alias.column" for joined outer schemas (v7.24).
        let idx = if !outer_alias.is_empty() && qual.eq_ignore_ascii_case(outer_alias) {
            ctx.columns
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        } else {
            None
        }
        .or_else(|| {
            let composite = alloc::format!("{qual}.{name}", name = c.name);
            ctx.columns
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(&composite))
        });
        if let Some(idx) = idx {
            let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
            if let Ok(lit) = value_to_literal_expr(v) {
                *e = lit;
                return;
            }
        }
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_in_expr(call, row, ctx, outer_alias);
            for o in order_by.iter_mut() {
                substitute_in_expr(&mut o.expr, row, ctx, outer_alias);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_in_expr(lhs, row, ctx, outer_alias);
            substitute_in_expr(rhs, row, ctx, outer_alias);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            substitute_in_expr(pattern, row, ctx, outer_alias);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
        }
        Expr::Extract { source, .. } => substitute_in_expr(source, row, ctx, outer_alias),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
            for p in partition_by {
                substitute_in_expr(p, row, ctx, outer_alias);
            }
            for (o, _, _) in order_by {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
        }
        Expr::ScalarSubquery(s) => substitute_in_select(s, row, ctx, outer_alias),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            substitute_in_select(subquery, row, ctx, outer_alias);
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
        Expr::Array(items) => {
            for elem in items {
                substitute_in_expr(elem, row, ctx, outer_alias);
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_in_expr(target, row, ctx, outer_alias);
            substitute_in_expr(index, row, ctx, outer_alias);
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            substitute_in_expr(array, row, ctx, outer_alias);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
            for (w, t) in branches {
                substitute_in_expr(w, row, ctx, outer_alias);
                substitute_in_expr(t, row, ctx, outer_alias);
            }
            if let Some(e) = else_branch {
                substitute_in_expr(e, row, ctx, outer_alias);
            }
        }
    }
}

/// v4.22: encode a Row to a comparable byte key for UNION-DISTINCT
/// dedup inside the recursive iteration. Crude but deterministic
/// — Debug prints embed type discriminants so NULL ≠ "" ≠ 0.
fn encode_row_key(row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for v in &row.values {
        let s = alloc::format!("{v:?}|");
        out.extend_from_slice(s.as_bytes());
    }
    out
}

fn select_has_window(stmt: &SelectStatement) -> bool {
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_has_window(expr)
        {
            return true;
        }
    }
    false
}

fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::WindowFunction { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_window(call) || order_by.iter().any(|o| expr_has_window(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_window(lhs) || expr_has_window(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_window(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_window),
        Expr::Like { expr, pattern, .. } => expr_has_window(expr) || expr_has_window(pattern),
        Expr::Extract { source, .. } => expr_has_window(source),
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_window),
        Expr::ArraySubscript { target, index } => expr_has_window(target) || expr_has_window(index),
        Expr::AnyAll { expr, array, .. } => expr_has_window(expr) || expr_has_window(array),
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_window)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_window(w) || expr_has_window(t))
                || else_branch.as_deref().is_some_and(expr_has_window)
        }
    }
}

fn collect_window_nodes(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::WindowFunction { .. } = e {
        // Deduplicate by structural equality on the expression
        // (cheap because window args + partition + order are
        // small). Without dedup we'd recompute identical windows
        // once per occurrence in the projection.
        if !out.iter().any(|x| x == e) {
            out.push(e.clone());
        }
        return;
    }
    match e {
        // Already handled by the early-return at the top.
        Expr::WindowFunction { .. } => unreachable!(),
        Expr::Binary { lhs, rhs, .. } => {
            collect_window_nodes(lhs, out);
            collect_window_nodes(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_window_nodes(expr, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_window_nodes(a, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_window_nodes(expr, out);
            collect_window_nodes(pattern, out);
        }
        Expr::Extract { source, .. } => collect_window_nodes(source, out),
        _ => {}
    }
}

fn rewrite_window_to_columns(e: &mut Expr, window_nodes: &[Expr]) {
    if let Expr::WindowFunction { .. } = e
        && let Some(idx) = window_nodes.iter().position(|w| w == e)
    {
        *e = Expr::Column(spg_sql::ast::ColumnName {
            qualifier: None,
            name: alloc::format!("__win_{idx}"),
        });
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_window_to_columns(lhs, window_nodes);
            rewrite_window_to_columns(rhs, window_nodes);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_window_to_columns(a, window_nodes);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
            rewrite_window_to_columns(pattern, window_nodes);
        }
        Expr::Extract { source, .. } => rewrite_window_to_columns(source, window_nodes),
        _ => {}
    }
}

/// Total order over partition-key tuples. NULL sorts as the
/// lowest value (matches the `<` partial order's NULL-last
/// behaviour with `INFINITY` flipped).
fn partition_key_cmp(a: &[Value], b: &[Value]) -> core::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = value_cmp(x, y);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn order_key_cmp(
    a: &[(Value, bool, Option<bool>)],
    b: &[(Value, bool, Option<bool>)],
) -> core::cmp::Ordering {
    // v7.24.1 — per-key DESC + effective NULLS placement (shared
    // contract with order_by_value_cmp).
    for ((va, desc, nf), (vb, _, _)) in a.iter().zip(b.iter()) {
        let c = order_by_value_cmp(*desc, *nf, va, vb);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// v7.17.0 Phase 3.10 — true when the Value is one of the
/// integer-shaped variants `generate_series` accepts as a start
/// / stop / step component. Float / NUMERIC are rejected — PG's
/// `generate_series(numeric, numeric)` overload is out of v7.17
/// scope.
const fn value_is_integer(v: &Value) -> bool {
    matches!(v, Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_))
}

/// v7.17.0 Phase 3.10 — widen any integer-shaped Value to i64 for
/// the generate_series iteration loop. Non-integer inputs panic;
/// caller guards via `value_is_integer`.
const fn value_to_i64(v: &Value) -> i64 {
    match v {
        Value::SmallInt(n) => *n as i64,
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        _ => panic!("value_to_i64 called on non-integer Value"),
    }
}

/// v7.17.0 Phase 3.10 — integer-mode generate_series materialiser.
/// Step direction follows the sign: positive step iterates upward
/// (stops when current > stop); negative iterates downward; zero
/// errors. Caller-facing row stream is `BigInt`-typed so a single
/// projection schema covers SmallInt / Int / BigInt callers.
fn generate_series_integers(
    start: i64,
    stop: i64,
    step: i64,
    cancel: &CancelToken<'_>,
) -> Result<alloc::vec::Vec<Row>, EngineError> {
    if step == 0 {
        return Err(EngineError::Unsupported(
            "generate_series(): step argument cannot be zero".into(),
        ));
    }
    let mut out = alloc::vec::Vec::new();
    let mut cur = start;
    // Hard cap to keep a runaway call from eating all memory. PG
    // has no such cap but does honour query timeout; SPG's cancel
    // token will fire too — this is a defense-in-depth backstop.
    const MAX_ROWS: usize = 10_000_000;
    loop {
        cancel.check()?;
        if step > 0 && cur > stop {
            break;
        }
        if step < 0 && cur < stop {
            break;
        }
        out.push(Row::new(alloc::vec![Value::BigInt(cur)]));
        if out.len() > MAX_ROWS {
            return Err(EngineError::Unsupported(alloc::format!(
                "generate_series(): exceeded {MAX_ROWS} rows; \
                 narrow start/stop or use a larger step"
            )));
        }
        cur = match cur.checked_add(step) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(out)
}

/// v7.17.0 Phase 3.10 — timestamp-mode generate_series. step is a
/// `Value::Interval { months, micros }` per the caller's guard;
/// each iteration adds the interval via `apply_binary_interval`
/// so month-shifting handles short-month rollover (PG semantics).
fn generate_series_timestamps(
    start: i64,
    stop: i64,
    step: Value,
    cancel: &CancelToken<'_>,
) -> Result<alloc::vec::Vec<Row>, EngineError> {
    let (months, micros) = match &step {
        Value::Interval { months, micros } => (*months, *micros),
        _ => unreachable!("caller guards step.is_interval"),
    };
    if months == 0 && micros == 0 {
        return Err(EngineError::Unsupported(
            "generate_series(): INTERVAL step cannot be zero".into(),
        ));
    }
    let ascending = months > 0 || micros > 0;
    let mut out = alloc::vec::Vec::new();
    let mut cur = Value::Timestamp(start);
    const MAX_ROWS: usize = 10_000_000;
    loop {
        cancel.check()?;
        let cur_t = match cur {
            Value::Timestamp(t) => t,
            _ => unreachable!("loop invariant: cur is Timestamp"),
        };
        if ascending && cur_t > stop {
            break;
        }
        if !ascending && cur_t < stop {
            break;
        }
        out.push(Row::new(alloc::vec![Value::Timestamp(cur_t)]));
        if out.len() > MAX_ROWS {
            return Err(EngineError::Unsupported(alloc::format!(
                "generate_series(): exceeded {MAX_ROWS} rows; \
                 narrow start/stop or use a larger step"
            )));
        }
        let next = eval::apply_binary_interval(
            spg_sql::ast::BinOp::Add,
            &cur,
            &Value::Interval { months, micros },
        )
        .map_err(EngineError::Eval)?;
        cur = match next {
            Some(v) => v,
            None => break,
        };
    }
    Ok(out)
}

#[allow(clippy::match_same_arms)] // explicit arms per type document the supported pairs
/// v7.24 (round-16 A) — per-key ORDER BY comparator honouring DESC
/// and the effective NULLS placement (explicit NULLS FIRST/LAST,
/// else the PG default: NULLS LAST for ASC, NULLS FIRST for DESC).
/// NULL placement is absolute — it does not flip with DESC.
pub(crate) fn order_by_value_cmp(
    desc: bool,
    nulls_first: Option<bool>,
    a: &Value,
    b: &Value,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let nf = nulls_first.unwrap_or(desc);
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nf {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nf {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let c = value_cmp(a, b);
            if desc { c.reverse() } else { c }
        }
    }
}

fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::SmallInt(x), Value::SmallInt(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        // Cross-type compare: fall back to the debug rendering —
        // same-partition is the goal, exact order is irrelevant.
        _ => alloc::format!("{a:?}").cmp(&alloc::format!("{b:?}")),
    }
}

/// Compute the window function's per-row output for one partition.
/// `slice` has (partition key, order key, original-row-index)
/// tuples already sorted by order key. `filtered_rows` is the
/// full row list indexed by original-row-index. `out_vals` is
/// the destination, also indexed by original-row-index.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::match_same_arms
)]
fn compute_window_partition(
    name: &str,
    args: &[Expr],
    ordered: bool,
    frame: Option<&WindowFrame>,
    null_treatment: spg_sql::ast::NullTreatment,
    slice: &[(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)],
    filtered_rows: &[&Row],
    ctx: &EvalContext<'_>,
    out_vals: &mut [Value],
) -> Result<(), EngineError> {
    let ignore_nulls = matches!(null_treatment, spg_sql::ast::NullTreatment::Ignore);
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "row_number" => {
            for (rank, (_, _, idx)) in slice.iter().enumerate() {
                out_vals[*idx] = Value::BigInt((rank + 1) as i64);
            }
            Ok(())
        }
        "rank" => {
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = (i + 1) as i64;
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "dense_rank" => {
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 0;
            for (_, okey, idx) in slice {
                if prev_key.is_none_or(|p| order_key_cmp(p, okey) != core::cmp::Ordering::Equal) {
                    current_rank += 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "sum" | "avg" | "min" | "max" | "count" | "count_star" => {
            // Pre-evaluate the function arg per row in the slice
            // (count_star has no arg).
            let arg_values: Vec<Value> = if lower == "count_star" || args.is_empty() {
                slice.iter().map(|_| Value::Null).collect()
            } else {
                slice
                    .iter()
                    .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                    .collect::<Result<_, _>>()
                    .map_err(EngineError::Eval)?
            };
            // v4.20: pick the effective frame. Explicit frame
            // overrides the implicit default (running for ordered,
            // whole-partition for unordered).
            let eff = effective_frame(frame, ordered)?;
            #[allow(clippy::needless_range_loop)]
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
                let mut sum: f64 = 0.0;
                let mut count: i64 = 0;
                let mut min_v: Option<f64> = None;
                let mut max_v: Option<f64> = None;
                let mut row_count: i64 = 0;
                if lo <= hi {
                    for j in lo..=hi {
                        let v = &arg_values[j];
                        match lower.as_str() {
                            "count_star" => row_count += 1,
                            "count" => {
                                if !v.is_null() {
                                    count += 1;
                                }
                            }
                            _ => {
                                if let Some(x) = value_to_f64(v) {
                                    sum += x;
                                    count += 1;
                                    min_v = Some(min_v.map_or(x, |m| m.min(x)));
                                    max_v = Some(max_v.map_or(x, |m| m.max(x)));
                                }
                            }
                        }
                    }
                }
                let value = match lower.as_str() {
                    "count_star" => Value::BigInt(row_count),
                    "count" => Value::BigInt(count),
                    "sum" => Value::Float(sum),
                    "avg" => {
                        if count == 0 {
                            Value::Null
                        } else {
                            Value::Float(sum / count as f64)
                        }
                    }
                    "min" => min_v.map_or(Value::Null, Value::Float),
                    "max" => max_v.map_or(Value::Null, Value::Float),
                    _ => unreachable!(),
                };
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = value;
            }
            Ok(())
        }
        "lag" | "lead" => {
            // lag(expr [, offset [, default]])
            // lead(expr [, offset [, default]])
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let offset: i64 = if args.len() >= 2 {
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "{lower}() offset must be integer"
                        )));
                    }
                }
            } else {
                1
            };
            let default: Value = if args.len() >= 3 {
                eval::eval_expr(&args[2], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?
            } else {
                Value::Null
            };
            let values: Vec<Value> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let n = slice.len();
            for (i, (_, _, idx)) in slice.iter().enumerate() {
                let signed_offset = if lower == "lag" { -offset } else { offset };
                let v = if ignore_nulls {
                    // v6.4.2 — IGNORE NULLS: walk in the offset direction
                    // skipping NULL values; the `offset`-th non-NULL
                    // encountered is the result.
                    let step: i64 = if signed_offset >= 0 { 1 } else { -1 };
                    let needed: i64 = signed_offset.abs();
                    if needed == 0 {
                        values[i].clone()
                    } else {
                        let mut j: i64 = i as i64;
                        let mut hits: i64 = 0;
                        let mut found: Option<Value> = None;
                        loop {
                            j += step;
                            if j < 0 || j >= n as i64 {
                                break;
                            }
                            #[allow(clippy::cast_sign_loss)]
                            let v = &values[j as usize];
                            if !v.is_null() {
                                hits += 1;
                                if hits == needed {
                                    found = Some(v.clone());
                                    break;
                                }
                            }
                        }
                        found.unwrap_or_else(|| default.clone())
                    }
                } else {
                    let target_signed = i64::try_from(i).unwrap_or(i64::MAX) + signed_offset;
                    if target_signed < 0 || target_signed >= i64::try_from(n).unwrap_or(i64::MAX) {
                        default.clone()
                    } else {
                        #[allow(clippy::cast_sign_loss)]
                        {
                            values[target_signed as usize].clone()
                        }
                    }
                };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "first_value" | "last_value" | "nth_value" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let values: Vec<Value> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let nth: usize = if lower == "nth_value" {
                if args.len() < 2 {
                    return Err(EngineError::Unsupported(
                        "nth_value() requires (expr, n)".into(),
                    ));
                }
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                let raw = match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(
                            "nth_value() n must be integer".into(),
                        ));
                    }
                };
                if raw < 1 {
                    return Err(EngineError::Unsupported(
                        "nth_value() n must be >= 1".into(),
                    ));
                }
                #[allow(clippy::cast_sign_loss)]
                {
                    raw as usize
                }
            } else {
                0
            };
            let eff = effective_frame(frame, ordered)?;
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
                let (_, _, idx) = &slice[i];
                let v = if lo > hi {
                    Value::Null
                } else if ignore_nulls && matches!(lower.as_str(), "first_value" | "last_value") {
                    // v6.4.2 — IGNORE NULLS: skip NULL cells when
                    // selecting the boundary value within the frame.
                    if lower == "first_value" {
                        (lo..=hi)
                            .find_map(|j| {
                                let v = &values[j];
                                (!v.is_null()).then(|| v.clone())
                            })
                            .unwrap_or(Value::Null)
                    } else {
                        (lo..=hi)
                            .rev()
                            .find_map(|j| {
                                let v = &values[j];
                                (!v.is_null()).then(|| v.clone())
                            })
                            .unwrap_or(Value::Null)
                    }
                } else {
                    match lower.as_str() {
                        "first_value" => values[lo].clone(),
                        "last_value" => values[hi].clone(),
                        "nth_value" => {
                            let pos = lo + nth - 1;
                            if pos > hi {
                                Value::Null
                            } else {
                                values[pos].clone()
                            }
                        }
                        _ => unreachable!(),
                    }
                };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "ntile" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(
                    "ntile(n) requires an integer argument".into(),
                ));
            }
            let v = eval::eval_expr(&args[0], filtered_rows[slice[0].2], ctx)
                .map_err(EngineError::Eval)?;
            let bucket_count: i64 = match v {
                Value::SmallInt(n) => i64::from(n),
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                _ => {
                    return Err(EngineError::Unsupported(
                        "ntile() argument must be integer".into(),
                    ));
                }
            };
            if bucket_count < 1 {
                return Err(EngineError::Unsupported(
                    "ntile() argument must be >= 1".into(),
                ));
            }
            #[allow(clippy::cast_sign_loss)]
            let buckets = bucket_count as usize;
            let n = slice.len();
            // Each bucket gets `base` rows; the first `extras` buckets
            // get one extra. PG semantics.
            let base = n / buckets;
            let extras = n % buckets;
            let mut bucket: usize = 1;
            let mut remaining_in_bucket = if extras > 0 { base + 1 } else { base };
            let mut buckets_with_extra_remaining = extras;
            for (_, _, idx) in slice {
                if remaining_in_bucket == 0 {
                    bucket += 1;
                    buckets_with_extra_remaining = buckets_with_extra_remaining.saturating_sub(1);
                    remaining_in_bucket = if buckets_with_extra_remaining > 0 {
                        base + 1
                    } else {
                        base
                    };
                    // Edge: if base==0 and extras==0, all rows fit;
                    // shouldn't reach here, but guard anyway.
                    if remaining_in_bucket == 0 {
                        remaining_in_bucket = 1;
                    }
                }
                out_vals[*idx] = Value::BigInt(i64::try_from(bucket).unwrap_or(i64::MAX));
                remaining_in_bucket -= 1;
            }
            Ok(())
        }
        "percent_rank" => {
            // (rank - 1) / (n - 1) where rank is the standard RANK().
            // Single-row partitions get 0.
            let n = slice.len();
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = i64::try_from(i + 1).unwrap_or(i64::MAX);
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                #[allow(clippy::cast_precision_loss)]
                let pr = if n <= 1 {
                    0.0
                } else {
                    (current_rank - 1) as f64 / (n - 1) as f64
                };
                out_vals[*idx] = Value::Float(pr);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "cume_dist" => {
            // # rows up to and including this row's peer group / n.
            let n = slice.len();
            // First pass: find peer-group-end rank for each row.
            for i in 0..slice.len() {
                let peer_end = peer_group_end(slice, i);
                #[allow(clippy::cast_precision_loss)]
                let cd = (peer_end + 1) as f64 / n as f64;
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = Value::Float(cd);
            }
            Ok(())
        }
        other => Err(EngineError::Unsupported(alloc::format!(
            "window function {other:?} not supported (v4.21: row_number/rank/dense_rank/sum/avg/count/min/max/lag/lead/first_value/last_value/nth_value/ntile/percent_rank/cume_dist)"
        ))),
    }
}

/// v4.20: resolve the user-provided frame down to a normalised
/// `(kind, start, end)`. `None` means default — derive from
/// `ordered`: ordered ⇒ RANGE UNBOUNDED PRECEDING AND CURRENT ROW,
/// unordered ⇒ ROWS UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING.
/// Single-bound shorthand (e.g. `ROWS 5 PRECEDING`) normalises
/// end → CURRENT ROW per the PG spec.
fn effective_frame(
    frame: Option<&WindowFrame>,
    ordered: bool,
) -> Result<(FrameKind, FrameBound, FrameBound), EngineError> {
    match frame {
        None => {
            if ordered {
                Ok((
                    FrameKind::Range,
                    FrameBound::UnboundedPreceding,
                    FrameBound::CurrentRow,
                ))
            } else {
                Ok((
                    FrameKind::Rows,
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                ))
            }
        }
        Some(fr) => {
            let end = fr.end.clone().unwrap_or(FrameBound::CurrentRow);
            // Reject start > end (a few impossible combinations).
            if matches!(fr.start, FrameBound::UnboundedFollowing)
                || matches!(end, FrameBound::UnboundedPreceding)
            {
                return Err(EngineError::Unsupported(alloc::format!(
                    "invalid frame: start={:?} end={:?}",
                    fr.start,
                    end
                )));
            }
            // RANGE OFFSET PRECEDING / FOLLOWING needs value-typed
            // arithmetic on the ORDER BY key (e.g. `RANGE BETWEEN
            // INTERVAL '1 day' PRECEDING AND CURRENT ROW`). Not
            // implemented in v4.20.
            if fr.kind == FrameKind::Range
                && (matches!(
                    fr.start,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ) || matches!(
                    end,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ))
            {
                return Err(EngineError::Unsupported(
                    "RANGE with explicit offset bounds is not supported (v4.20: only UNBOUNDED / CURRENT ROW for RANGE)".into(),
                ));
            }
            Ok((fr.kind, fr.start.clone(), end))
        }
    }
}

/// Compute `(lo, hi)` row-index bounds inside the partition slice
/// for the row at position `i`. Inclusive, clamped to
/// `[0, slice.len()-1]`. Empty result if `lo > hi`.
#[allow(clippy::type_complexity)]
fn frame_bounds_for_row(
    eff: &(FrameKind, FrameBound, FrameBound),
    i: usize,
    slice: &[(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> (usize, usize) {
    let (kind, start, end) = eff;
    let n = slice.len();
    let last = n.saturating_sub(1);
    let (mut lo, mut hi) = match kind {
        FrameKind::Rows => {
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
            (lo, hi)
        }
        FrameKind::Range => {
            // RANGE bounds are peer-aware. With only UNBOUNDED and
            // CURRENT ROW supported (rejected at effective_frame for
            // explicit offsets), the start/end map to the
            // partition's full extent at the same-order-key peer
            // group boundary.
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_start(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE"),
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_end(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE"),
            };
            (lo, hi)
        }
    };
    if hi >= n {
        hi = last;
    }
    if lo >= n {
        lo = last;
    }
    (lo, hi)
}

/// Find the inclusive index of the first row with the same ORDER
/// BY key as `slice[i]`. Slice is already sorted by partition then
/// order, so peers are contiguous.
#[allow(clippy::type_complexity)]
fn peer_group_start(
    slice: &[(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)],
    i: usize,
) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j > 0 && order_key_cmp(&slice[j - 1].1, key) == core::cmp::Ordering::Equal {
        j -= 1;
    }
    j
}

/// Find the inclusive index of the last row with the same ORDER
/// BY key as `slice[i]`.
#[allow(clippy::type_complexity)]
fn peer_group_end(
    slice: &[(Vec<Value>, Vec<(Value, bool, Option<bool>)>, usize)],
    i: usize,
) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j + 1 < slice.len() && order_key_cmp(&slice[j + 1].1, key) == core::cmp::Ordering::Equal {
        j += 1;
    }
    j
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::Int(n) => Some(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    }
}

/// Quick scan for any subquery-bearing node in a SELECT's WHERE /
/// projection / `order_by` — saves cloning the AST when there are
/// none (the common case).
fn expr_tree_has_subquery(stmt: &SelectStatement) -> bool {
    let mut any = false;
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            any = any || expr_has_subquery(expr);
        }
    }
    if let Some(w) = &stmt.where_ {
        any = any || expr_has_subquery(w);
    }
    if let Some(h) = &stmt.having {
        any = any || expr_has_subquery(h);
    }
    for o in &stmt.order_by {
        any = any || expr_has_subquery(&o.expr);
    }
    for (_, peer) in &stmt.unions {
        any = any || expr_tree_has_subquery(peer);
    }
    any
}

pub(crate) fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_subquery(call) || order_by.iter().any(|o| expr_has_subquery(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_subquery(lhs) || expr_has_subquery(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_subquery(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Like { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Extract { source, .. } => expr_has_subquery(source),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(expr_has_subquery)
                || partition_by.iter().any(expr_has_subquery)
                || order_by.iter().any(|(e, _, _)| expr_has_subquery(e))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_subquery),
        Expr::ArraySubscript { target, index } => {
            expr_has_subquery(target) || expr_has_subquery(index)
        }
        Expr::AnyAll { expr, array, .. } => expr_has_subquery(expr) || expr_has_subquery(array),
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_subquery(w) || expr_has_subquery(t))
                || else_branch.as_deref().is_some_and(expr_has_subquery)
        }
    }
}

/// v4.10 helper: materialise a runtime `Value` back into an AST
/// `Expr::Literal` for the subquery-rewrite path. Supports the
/// types `Literal` can represent (Integer / Float / Text / Bool /
/// Null). Date / Timestamp / Numeric / Vector / Interval / JSON
/// would lose precision through Literal and aren't supported in
/// uncorrelated-subquery results; they error with a clear hint.
fn value_to_literal_expr(v: Value) -> Result<Expr, EngineError> {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s),
        Value::Bool(b) => Literal::Bool(b),
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "subquery result type {:?} not yet materialisable; cast to text or integer in the inner SELECT",
                other.data_type()
            )));
        }
    };
    Ok(Expr::Literal(lit))
}

/// v7.13.0 — wider helper used by `INSERT … SELECT` (mailrs
/// round-5 G4). Covers the most common `Value` variants. Types
/// that need lossy textual round-trip (BYTEA, arrays, ts*)
/// surface as an Unsupported error so the caller can add a cast
/// in the inner SELECT.
fn value_to_literal_expr_permissive(v: Value) -> Result<Expr, EngineError> {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s),
        Value::Bool(b) => Literal::Bool(b),
        Value::Vector(xs) => Literal::Vector(xs),
        // Date / Timestamp / Timestamptz / Numeric round-trip
        // through a TEXT literal that `coerce_value` re-parses
        // against the target column type.
        Value::Date(days) => {
            let micros = (i64::from(days)) * 86_400_000_000;
            Literal::String(format_timestamp_micros_as_date(micros))
        }
        Value::Timestamp(us) => Literal::String(format_timestamp_micros(us)),
        Value::Numeric { scaled, scale } => Literal::String(format_numeric(scaled, scale)),
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "INSERT … SELECT cannot materialise value of type {:?}; \
                 add an explicit CAST in the inner SELECT",
                other.data_type()
            )));
        }
    };
    Ok(Expr::Literal(lit))
}

fn format_timestamp_micros(us: i64) -> String {
    // Same Y/M/D split used by the wire layer; epoch-relative.
    let days = us.div_euclid(86_400_000_000);
    let intra_day = us.rem_euclid(86_400_000_000);
    let date = format_timestamp_micros_as_date(days * 86_400_000_000);
    let secs = intra_day / 1_000_000;
    let us_rem = intra_day % 1_000_000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    if us_rem == 0 {
        alloc::format!("{date} {h:02}:{m:02}:{s:02}")
    } else {
        alloc::format!("{date} {h:02}:{m:02}:{s:02}.{us_rem:06}")
    }
}

fn format_timestamp_micros_as_date(us: i64) -> String {
    // Days since 1970-01-01 → calendar Y-M-D via the proleptic
    // Gregorian conversion used by spg-engine's date helpers.
    let days = us.div_euclid(86_400_000_000);
    // 1970-01-01 = JDN 2440588.
    let jdn = days + 2_440_588;
    let (y, mo, d) = jdn_to_ymd(jdn);
    alloc::format!("{y:04}-{mo:02}-{d:02}")
}

fn jdn_to_ymd(jdn: i64) -> (i64, u32, u32) {
    // Fliegel & Van Flandern (1968) — works for all positive JDNs.
    let l = jdn + 68569;
    let n = (4 * l) / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = (4000 * (l + 1)) / 1_461_001;
    let l = l - (1461 * i) / 4 + 31;
    let j = (80 * l) / 2447;
    let day = (l - (2447 * j) / 80) as u32;
    let l = j / 11;
    let month = (j + 2 - 12 * l) as u32;
    let year = 100 * (n - 49) + i + l;
    (year, month, day)
}

fn format_numeric(scaled: i128, scale: u8) -> String {
    if scale == 0 {
        return alloc::format!("{scaled}");
    }
    let abs = scaled.unsigned_abs();
    let divisor = 10u128.pow(u32::from(scale));
    let whole = abs / divisor;
    let frac = abs % divisor;
    let sign = if scaled < 0 { "-" } else { "" };
    alloc::format!("{sign}{whole}.{frac:0width$}", width = usize::from(scale))
}

/// v6.1.1 — walk the prepared `Statement` AST and replace every
/// `Expr::Placeholder(n)` with `Expr::Literal(value_to_literal(
/// params[n-1]))`. The dispatch downstream sees a `Statement`
/// indistinguishable from a simple-query parse, so the exec path
/// stays unchanged.
///
/// Errors fall into one shape: a `$N` references past the bound
/// `params.len()`. Out-of-range happens when the Bind didn't
/// supply enough values; pgwire surfaces this as a protocol error
/// to the client.
/// v7.15.0 — rewrite every (potentially-qualified) column
/// identifier matching `old` to `new` in a stored SQL source
/// string. Used by `ALTER TABLE … RENAME COLUMN` to patch
/// CHECK predicate sources, partial-index predicate sources,
/// and runtime DEFAULT expression sources before they get
/// re-parsed on the next INSERT/UPDATE.
///
/// Round-trips through the parser, so the rewritten output is
/// the canonical Display form (matches what the engine stores
/// for fresh predicates). If the source doesn't parse, surfaces
/// the parse error — the invariant that stored predicates are
/// in canonical Display form means a parse failure here is a
/// real bug, not a user mistake to swallow.
fn rewrite_column_in_source(
    src: &str,
    old: &str,
    new: &str,
) -> Result<alloc::string::String, EngineError> {
    let mut expr = spg_sql::parser::parse_expression(src).map_err(|e| {
        EngineError::Unsupported(alloc::format!(
            "ALTER TABLE RENAME COLUMN: stored predicate source {src:?} \
             failed to parse for rewrite ({e})"
        ))
    })?;
    rewrite_column_in_expr(&mut expr, old, new);
    Ok(alloc::format!("{expr}"))
}

/// v7.15.0 — Expr walker that swaps `Expr::Column { name: old, .. }`
/// for `Expr::Column { name: new, .. }`. Qualifier is preserved
/// (e.g. `t.old` → `t.new`); a foreign-table qualifier still
/// gets rewritten because the AST has no way to tell us this
/// predicate is on table T versus table T2 — predicate sources
/// in SPG are always scoped to the owning table, so any
/// qualifier present is either redundant or wrong.
fn rewrite_column_in_expr(e: &mut Expr, old: &str, new: &str) {
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_column_in_expr(call, old, new);
            for o in order_by.iter_mut() {
                rewrite_column_in_expr(&mut o.expr, old, new);
            }
        }
        Expr::Column(c) => {
            if c.name.eq_ignore_ascii_case(old) {
                c.name = new.to_string();
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_column_in_expr(lhs, old, new);
            rewrite_column_in_expr(rhs, old, new);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_column_in_expr(expr, old, new);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_column_in_expr(a, old, new);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_column_in_expr(expr, old, new);
            rewrite_column_in_expr(pattern, old, new);
        }
        Expr::Extract { source, .. } => rewrite_column_in_expr(source, old, new),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_column_in_expr(a, old, new);
            }
            for p in partition_by {
                rewrite_column_in_expr(p, old, new);
            }
            for (o, _, _) in order_by {
                rewrite_column_in_expr(o, old, new);
            }
        }
        Expr::Array(items) => {
            for elem in items {
                rewrite_column_in_expr(elem, old, new);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_column_in_expr(target, old, new);
            rewrite_column_in_expr(index, old, new);
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_column_in_expr(expr, old, new);
            rewrite_column_in_expr(array, old, new);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_column_in_expr(o, old, new);
            }
            for (w, t) in branches {
                rewrite_column_in_expr(w, old, new);
                rewrite_column_in_expr(t, old, new);
            }
            if let Some(e) = else_branch {
                rewrite_column_in_expr(e, old, new);
            }
        }
        // Stored predicate sources never contain subqueries —
        // CHECK / partial-index / runtime_default are all scalar.
        // If a future feature changes that, recurse here.
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {}
        Expr::Literal(_) | Expr::Placeholder(_) => {}
    }
}

/// v7.16.0 — walks a parsed statement and replaces every
/// `Expr::Placeholder(N)` with the corresponding `params[N-1]`
/// re-encoded as an `Expr::Literal`. Used internally by
/// `Engine::execute_prepared` AND surfaced for the spg-embedded
/// WAL path (which needs the bind-final AST so replay sees a
/// simple-query-shaped statement, not a `$1`-shaped one). Errors
/// when a placeholder references an index past the params slice.
pub fn substitute_placeholders(stmt: &mut Statement, params: &[Value]) -> Result<(), EngineError> {
    match stmt {
        Statement::Select(s) => substitute_select(s, params)?,
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    substitute_expr(e, params)?;
                }
            }
            // ON CONFLICT DO UPDATE assignments / WHERE can carry
            // placeholders too (`… DO UPDATE SET reason = $2` —
            // mailrs embed round-12).
            if let Some(clause) = &mut ins.on_conflict
                && let spg_sql::ast::OnConflictAction::Update {
                    assignments,
                    where_,
                } = &mut clause.action
            {
                for (_, e) in assignments.iter_mut() {
                    substitute_expr(e, params)?;
                }
                if let Some(w) = where_ {
                    substitute_expr(w, params)?;
                }
            }
        }
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                substitute_expr(e, params)?;
            }
            if let Some(w) = &mut u.where_ {
                substitute_expr(w, params)?;
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &mut d.where_ {
                substitute_expr(w, params)?;
            }
        }
        Statement::Explain(e) => substitute_select(&mut e.inner, params)?,
        // Other statements (CREATE / BEGIN / SHOW / …) have no
        // expression slots; no walk needed.
        _ => {}
    }
    Ok(())
}

/// v7.25.1 (mailrs round-18) — THE canonical mutable traversal of
/// every expression slot in a SelectStatement, including every
/// nested SelectStatement (CTE bodies, UNION peers, LATERAL derived
/// tables) and the JOIN ON conditions. Round-12 #7b and round-18
/// were both "a hand-rolled Select walker forgot one subtree";
/// every whole-statement rewrite pass (placeholders, clock) must go
/// through here so a new AST slot only needs adding once.
/// Expression-INTERNAL recursion (into subquery nodes inside an
/// Expr) stays the visitor's own responsibility.
pub(crate) fn walk_select_exprs_mut(
    s: &mut SelectStatement,
    f: &mut impl FnMut(&mut Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    for cte in &mut s.ctes {
        walk_select_exprs_mut(&mut cte.body, f)?;
    }
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            f(expr)?;
        }
    }
    if let Some(from) = &mut s.from {
        if let Some(sub) = &mut from.primary.lateral_subquery {
            walk_select_exprs_mut(sub, f)?;
        }
        for j in &mut from.joins {
            if let Some(sub) = &mut j.table.lateral_subquery {
                walk_select_exprs_mut(sub, f)?;
            }
            if let Some(on) = &mut j.on {
                f(on)?;
            }
        }
    }
    if let Some(w) = &mut s.where_ {
        f(w)?;
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            f(g)?;
        }
    }
    if let Some(h) = &mut s.having {
        f(h)?;
    }
    for o in &mut s.order_by {
        f(&mut o.expr)?;
    }
    for (_, peer) in &mut s.unions {
        walk_select_exprs_mut(peer, f)?;
    }
    Ok(())
}

fn substitute_select(s: &mut SelectStatement, params: &[Value]) -> Result<(), EngineError> {
    walk_select_exprs_mut(s, &mut |e| substitute_expr(e, params))?;
    // v7.25.1 — LIMIT/OFFSET placeholders inside CTE bodies and
    // UNION peers resolve through their own recursion (the walker
    // above only visits Expr slots), so handle them per nested
    // statement here.
    for cte in &mut s.ctes {
        resolve_limit_offset_placeholders(&mut cte.body, params)?;
    }
    for (_, peer) in &mut s.unions {
        resolve_limit_offset_placeholders(peer, params)?;
    }
    // v7.9.24 — LIMIT $N / OFFSET $N placeholder resolution.
    // mailrs H2. After this pass each LIMIT/OFFSET that was a
    // Placeholder is rewritten to Literal so the existing
    // `LimitExpr::as_literal` path consumes a concrete u32.
    if let Some(le) = s.limit {
        s.limit = Some(resolve_limit_placeholder(le, params)?);
    }
    if let Some(le) = s.offset {
        s.offset = Some(resolve_limit_placeholder(le, params)?);
    }
    Ok(())
}

/// v7.25.1 — recursive LIMIT/OFFSET placeholder resolution for
/// nested statements (CTE bodies / UNION peers).
fn resolve_limit_offset_placeholders(
    s: &mut SelectStatement,
    params: &[Value],
) -> Result<(), EngineError> {
    if let Some(le) = s.limit {
        s.limit = Some(resolve_limit_placeholder(le, params)?);
    }
    if let Some(le) = s.offset {
        s.offset = Some(resolve_limit_placeholder(le, params)?);
    }
    for cte in &mut s.ctes {
        resolve_limit_offset_placeholders(&mut cte.body, params)?;
    }
    for (_, peer) in &mut s.unions {
        resolve_limit_offset_placeholders(peer, params)?;
    }
    Ok(())
}

fn resolve_limit_placeholder(
    le: spg_sql::ast::LimitExpr,
    params: &[Value],
) -> Result<spg_sql::ast::LimitExpr, EngineError> {
    use spg_sql::ast::LimitExpr;
    match le {
        LimitExpr::Literal(_) => Ok(le),
        LimitExpr::Placeholder(n) => {
            let idx = usize::from(n).saturating_sub(1);
            let v = params.get(idx).ok_or_else(|| {
                EngineError::Eval(EvalError::PlaceholderOutOfRange {
                    n,
                    bound: u16::try_from(params.len()).unwrap_or(u16::MAX),
                })
            })?;
            let int = match v {
                Value::SmallInt(x) => Some(i64::from(*x)),
                Value::Int(x) => Some(i64::from(*x)),
                Value::BigInt(x) => Some(*x),
                _ => None,
            }
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} bound to non-integer {v:?}"
                ))
            })?;
            if int < 0 {
                return Err(EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} bound to negative value {int}"
                )));
            }
            let bounded = u32::try_from(int).map_err(|_| {
                EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} value {int} exceeds u32 range"
                ))
            })?;
            Ok(LimitExpr::Literal(bounded))
        }
    }
}

fn substitute_expr(e: &mut Expr, params: &[Value]) -> Result<(), EngineError> {
    if let Expr::Placeholder(n) = e {
        let idx = usize::from(*n).saturating_sub(1);
        let v = params.get(idx).ok_or_else(|| {
            EngineError::Eval(EvalError::PlaceholderOutOfRange {
                n: *n,
                bound: u16::try_from(params.len()).unwrap_or(u16::MAX),
            })
        })?;
        *e = Expr::Literal(value_to_literal(v.clone()));
        return Ok(());
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_expr(call, params)?;
            for o in order_by.iter_mut() {
                substitute_expr(&mut o.expr, params)?;
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_expr(lhs, params)?;
            substitute_expr(rhs, params)?;
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            substitute_expr(expr, params)?;
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_expr(a, params)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_expr(expr, params)?;
            substitute_expr(pattern, params)?;
        }
        Expr::Extract { source, .. } => substitute_expr(source, params)?,
        Expr::ScalarSubquery(s) => substitute_select(s, params)?,
        Expr::Exists { subquery, .. } => substitute_select(subquery, params)?,
        Expr::InSubquery { expr, subquery, .. } => {
            substitute_expr(expr, params)?;
            substitute_select(subquery, params)?;
        }
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_expr(a, params)?;
            }
            for p in partition_by {
                substitute_expr(p, params)?;
            }
            for (e, _, _) in order_by {
                substitute_expr(e, params)?;
            }
        }
        Expr::Literal(_) | Expr::Column(_) => {}
        // Already handled above.
        Expr::Placeholder(_) => unreachable!("Placeholder handled at top of fn"),
        Expr::Array(items) => {
            for elem in items {
                substitute_expr(elem, params)?;
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_expr(target, params)?;
            substitute_expr(index, params)?;
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_expr(expr, params)?;
            substitute_expr(array, params)?;
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_expr(o, params)?;
            }
            for (w, t) in branches {
                substitute_expr(w, params)?;
                substitute_expr(t, params)?;
            }
            if let Some(e) = else_branch {
                substitute_expr(e, params)?;
            }
        }
    }
    Ok(())
}

/// v6.1.1 — convert a runtime `Value` into the closest matching
/// `Literal` for the substitute walker. Lossless for the simple
/// scalars (Int / Float / Text / Bool); Numeric / Date / Timestamp
/// / Json / Interval render as their canonical text form so the
/// downstream coerce_value can re-parse against the target column
/// type. SQ8 / HalfVector cells are NOT expected as bind params;
/// pgwire's Bind decodes vector params to the f32 representation
/// before they reach this helper.
/// v6.2.0 — total ordering on `Value`s used by ANALYZE to sort a
/// column's non-NULL sample before histogram building. Cross-type
/// pairs (Int vs Float, Date vs Timestamp, …) compare via the
/// same widening the eval-side `compare` operator uses; everything
/// else (the genuinely-incompatible pairs) falls back to ordering
/// by canonical string form so the sort is still total + stable.
/// Vector / SQ8 / Half / Json / Numeric / Interval values reach
/// here only via the string-fallback path because vector columns
/// are filtered out upstream.
fn sort_values_for_histogram(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::SmallInt(a), Value::SmallInt(b)) => a.cmp(b),
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (Value::SmallInt(a), Value::Int(b)) => i32::from(*a).cmp(b),
        (Value::Int(a), Value::SmallInt(b)) => a.cmp(&i32::from(*b)),
        (Value::Int(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::SmallInt(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::SmallInt(b)) => a.cmp(&i64::from(*b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Text(a), Value::Text(b)) | (Value::Json(a), Value::Json(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        // Mixed numeric/float — widen to f64 and compare.
        (Value::SmallInt(n), Value::Float(x)) => {
            (f64::from(*n)).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::SmallInt(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        (Value::Int(n), Value::Float(x)) => {
            (f64::from(*n)).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::Int(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        (Value::BigInt(n), Value::Float(x)) => {
            #[allow(clippy::cast_precision_loss)]
            let nf = *n as f64;
            nf.partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::BigInt(n)) => {
            #[allow(clippy::cast_precision_loss)]
            let nf = *n as f64;
            x.partial_cmp(&nf).unwrap_or(Ordering::Equal)
        }
        // Cross-type fallback: lexicographic on canonical form.
        // Total + stable so the sort is well-defined.
        _ => canonical_value_repr(a).cmp(&canonical_value_repr(b)),
    }
}

/// v6.2.0 — render the histogram bounds list as a `[v0, v1, ...]`
/// string for the `spg_statistic.histogram_bounds` column. Values
/// containing `,` or `[` / `]` are JSON-style escaped so the
/// rendering round-trips through a future parser; v6.2.0 only
/// uses the rendered form for human consumption, so the escaping
/// is conservative.
fn render_histogram_bounds(bounds: &[alloc::string::String]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bounds.len() * 8 + 2);
    out.push('[');
    for (i, b) in bounds.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let needs_quote = b.contains([',', '[', ']', '"']) || b.is_empty();
        if needs_quote {
            out.push('"');
            for ch in b.chars() {
                if ch == '"' || ch == '\\' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(b);
        }
    }
    out.push(']');
    out
}

/// v6.2.0 — canonical textual form of a `Value` for histogram
/// bound storage. Strings used by ANALYZE for sort + bound output.
/// INT / BIGINT → decimal; FLOAT → shortest-round-trip via
/// `{:?}`; TEXT pass-through; BOOL → `t` / `f`; DATE / TIMESTAMP →
/// the same form `format_date` / `format_timestamp` produce for
/// SQL Display. Vector / SQ8 / Half / Json / Numeric / Interval
/// reach this only via a non-Vector column (vector columns are
/// skipped upstream); they fall back to a Debug-derived form so
/// stats still serialise without crashing.
pub(crate) fn canonical_value_repr(v: &Value) -> alloc::string::String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::SmallInt(n) => alloc::format!("{n}"),
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        Value::Float(x) => alloc::format!("{x:?}"),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::Date(d) => eval::format_date(*d),
        Value::Timestamp(t) => eval::format_timestamp(*t),
        // v7.17.0 Phase 3.P0-32 — PG TIME canonical text form.
        Value::Time(us) => eval::format_time(*us),
        // v7.17.0 Phase 3.P0-33 — MySQL YEAR 4-digit zero-padded.
        Value::Year(y) => alloc::format!("{y:04}"),
        // v7.17.0 Phase 3.P0-34 — PG TIMETZ canonical text form.
        Value::TimeTz { us, offset_secs } => eval::format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-35 — PG MONEY canonical en_US text form.
        Value::Money(c) => eval::format_money(*c),
        // v7.17.0 Phase 3.P0-38 — PG range canonical text form.
        v @ Value::Range { .. } => format_range_str(v),
        // v7.17.0 Phase 3.P0-39 — PG hstore canonical text form.
        Value::Hstore(pairs) => format_hstore_str(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D array canonical text form.
        Value::IntArray2D(rows) => format_int_2d_text(rows),
        Value::BigIntArray2D(rows) => format_bigint_2d_text(rows),
        Value::TextArray2D(rows) => format_text_2d_text(rows),
        Value::Interval { months, micros } => eval::format_interval(*months, *micros),
        Value::Numeric { scaled, scale } => eval::format_numeric(*scaled, *scale),
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            // Unreachable in practice (vector columns are filtered
            // out before this). Defensive fallback so a future
            // vector-stats path doesn't crash.
            alloc::format!("{v:?}")
        }
        // v7.5.0 — Value is #[non_exhaustive] for downstream
        // forward-compat. Future variants fall through to Debug
        // form here (same shape as the vector fallback above).
        _ => alloc::format!("{v:?}"),
    }
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

fn value_to_literal(v: Value) -> Literal {
    match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s),
        Value::Bool(b) => Literal::Bool(b),
        Value::Vector(v) => Literal::Vector(v),
        Value::Numeric { scaled, scale } => Literal::String(eval::format_numeric(scaled, scale)),
        Value::Date(d) => Literal::String(eval::format_date(d)),
        Value::Timestamp(t) => Literal::String(eval::format_timestamp(t)),
        // v7.17.0 Phase 3.P0-69 — UUID round-trips via canonical
        // hyphenated text. Without this arm the fallback below
        // renders `Debug` form ("Uuid([85, …])") which the
        // engine's Text → Uuid coerce can't parse, breaking
        // prepared-bind round-trip from the spg-sqlx adapter.
        Value::Uuid(b) => Literal::String(spg_storage::format_uuid(&b)),
        // v7.16.0 — BYTEA round-trip for the spg-sqlx Bind path.
        // PG-canonical text rep is `\x` + lowercase hex; the
        // engine's coerce_value already accepts that on the
        // text → bytea direction.
        Value::Bytes(b) => Literal::String(eval::format_bytea_hex(&b)),
        // Arrays ride the AST natively (mailrs embed round-12) —
        // the prior `{a,b,c}` text form only worked where a column
        // type drove the re-parse; `= ANY($1)` has no column
        // context and saw a bare Text value.
        Value::TextArray(items) => Literal::TextArray(items),
        Value::IntArray(items) => Literal::IntArray(items),
        Value::BigIntArray(items) => Literal::BigIntArray(items),
        Value::Interval { months, micros } => Literal::Interval {
            months,
            micros,
            text: eval::format_interval(months, micros),
        },
        // SQ8 / halfvec cells dequantise to f32 before reaching the
        // substitute walker; pgwire's Bind path handles that.
        Value::Sq8Vector(q) => Literal::Vector(spg_storage::quantize::dequantize(&q)),
        Value::HalfVector(h) => Literal::Vector(h.to_f32_vec()),
        // v7.5.0 — Value is #[non_exhaustive]; future variants
        // render as Debug-form String literal until explicit
        // mapping is added.
        v => Literal::String(alloc::format!("{v:?}")),
    }
}

fn rewrite_clock_calls(stmt: &mut Statement, now_micros: Option<i64>) {
    let Some(now) = now_micros else {
        return;
    };
    match stmt {
        Statement::Select(s) => rewrite_select_clock(s, now),
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    rewrite_expr_clock(e, now);
                }
            }
            // `ON CONFLICT … DO UPDATE SET created_at = NOW()` —
            // the upsert assignments carry clock calls too (mailrs
            // embed round-12).
            if let Some(clause) = &mut ins.on_conflict
                && let spg_sql::ast::OnConflictAction::Update {
                    assignments,
                    where_,
                } = &mut clause.action
            {
                for (_, e) in assignments.iter_mut() {
                    rewrite_expr_clock(e, now);
                }
                if let Some(w) = where_ {
                    rewrite_expr_clock(w, now);
                }
            }
        }
        // `UPDATE … SET seen_at = NOW() WHERE …` / `DELETE … WHERE
        // ts < NOW()` (mailrs embed round-12 — previously only
        // SELECT / INSERT-rows were walked).
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                rewrite_expr_clock(e, now);
            }
            if let Some(w) = &mut u.where_ {
                rewrite_expr_clock(w, now);
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &mut d.where_ {
                rewrite_expr_clock(w, now);
            }
        }
        _ => {}
    }
}

fn rewrite_select_clock(s: &mut SelectStatement, now: i64) {
    // v7.25.1 (round-18) — shared traversal: CTE bodies, LATERAL
    // subqueries, JOIN ON, and UNION peers all get the clock
    // rewrite (NOW() inside a CTE previously survived to eval as
    // "unknown function `now`").
    let _ = walk_select_exprs_mut(s, &mut |e| {
        rewrite_expr_clock(e, now);
        Ok(())
    });
}

/// v3.0.3 hot path: every recursion lands in exactly one `match` arm.
/// Literal / Column-with-qualifier (the dominant cases on a typical
/// AST) take a single pattern dispatch and exit. The clock-rewrite
/// targets (zero-arg `NOW` / `CURRENT_TIMESTAMP` / `CURRENT_DATE`
/// functions, and bare `CURRENT_TIMESTAMP` / `CURRENT_DATE` column
/// refs) sit on their own arms with match guards so the fall-through
/// to the recursive arms is unambiguous.
fn rewrite_expr_clock(e: &mut Expr, now: i64) {
    // Fast-path test on the no-recursion shapes first. We can't fold
    // them into the big match below because they need to *replace* `e`
    // outright; the recursive arms below match on its sub-fields.
    if let Some(replacement) = clock_replacement_for(e, now) {
        *e = replacement;
        return;
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_expr_clock(call, now);
            for o in order_by.iter_mut() {
                rewrite_expr_clock(&mut o.expr, now);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr_clock(lhs, now);
            rewrite_expr_clock(rhs, now);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_expr_clock(expr, now);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_expr_clock(pattern, now);
        }
        Expr::Extract { source, .. } => rewrite_expr_clock(source, now),
        // v4.10 subquery nodes — recurse into the inner SELECT's
        // expression slots so e.g. SELECT NOW() in a scalar
        // subquery picks up the same instant as the outer query.
        Expr::ScalarSubquery(s) => rewrite_select_clock(s, now),
        Expr::Exists { subquery, .. } => rewrite_select_clock(subquery, now),
        Expr::InSubquery { expr, subquery, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_select_clock(subquery, now);
        }
        // v4.12 window functions — args + PARTITION BY + ORDER BY
        // may all reference clock literals.
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
            for p in partition_by {
                rewrite_expr_clock(p, now);
            }
            for (e, _, _) in order_by {
                rewrite_expr_clock(e, now);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
        Expr::Array(items) => {
            for elem in items {
                rewrite_expr_clock(elem, now);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_expr_clock(target, now);
            rewrite_expr_clock(index, now);
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_expr_clock(array, now);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_expr_clock(o, now);
            }
            for (w, t) in branches {
                rewrite_expr_clock(w, now);
                rewrite_expr_clock(t, now);
            }
            if let Some(e) = else_branch {
                rewrite_expr_clock(e, now);
            }
        }
    }
}

/// Returns `Some(Expr)` when `e` is one of the clock-call shapes that
/// must be rewritten; otherwise `None` so the caller falls through to
/// the recursive walk. Identifies both function-call forms (`NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()`) and bare-identifier forms
/// (`CURRENT_TIMESTAMP` / `CURRENT_DATE` as unqualified column refs,
/// which is how PG accepts them without parens).
fn clock_replacement_for(e: &Expr, now: i64) -> Option<Expr> {
    let (kind, name) = match e {
        Expr::FunctionCall { name, args } if args.is_empty() => (ClockSite::Fn, name.as_str()),
        Expr::Column(c) if c.qualifier.is_none() => (ClockSite::BareIdent, c.name.as_str()),
        _ => return None,
    };
    // ASCII case-insensitive name match. Each entry decides what
    // synthetic literal the call expands to.
    //
    // v7.17.0 Phase 3.P0-29 — `unix_timestamp` (no args) joins this
    // table as MySQL's epoch-seconds equivalent of `now()`. Folded
    // to a BigInt literal here so apply_function never needs a
    // clock dependency.
    enum ClockShape {
        Timestamp,
        Date,
        UnixSeconds,
    }
    let shape = match name.len() {
        3 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("now") => {
            Some(ClockShape::Timestamp)
        }
        12 if name.eq_ignore_ascii_case("current_date") => Some(ClockShape::Date),
        14 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("unix_timestamp") => {
            Some(ClockShape::UnixSeconds)
        }
        17 if name.eq_ignore_ascii_case("current_timestamp") => Some(ClockShape::Timestamp),
        _ => None,
    };
    let shape = shape?;
    let payload = match shape {
        ClockShape::Timestamp => now,
        ClockShape::Date => now.div_euclid(86_400_000_000),
        ClockShape::UnixSeconds => now.div_euclid(1_000_000),
    };
    let target = match shape {
        ClockShape::Timestamp => spg_sql::ast::CastTarget::Timestamp,
        ClockShape::Date => spg_sql::ast::CastTarget::Date,
        ClockShape::UnixSeconds => spg_sql::ast::CastTarget::BigInt,
    };
    Some(Expr::Cast {
        expr: alloc::boxed::Box::new(Expr::Literal(spg_sql::ast::Literal::Integer(payload))),
        target,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockSite {
    Fn,
    BareIdent,
}

/// `ORDER BY <integer>` references the N-th SELECT item (1-based).
/// Swap the integer literal for the matching item's expression so the
/// executor doesn't need a special-case branch. Recurses into UNION
/// peers because each peer keeps its own SELECT list.
/// v6.4.1 — expand `GROUP BY ALL` to every non-aggregate SELECT-list
/// item. Mirrors DuckDB / PG 19 semantics. Wildcards (`SELECT * …`)
/// are NOT expanded by GROUP BY ALL (PG 19 leaves the wildcard intact
/// and groups by whatever explicit non-aggregates remain — none in
/// the wildcard-only case, which still works for non-aggregate
/// queries).
fn expand_group_by_all(s: &mut SelectStatement) {
    if !s.group_by_all {
        for (_, peer) in &mut s.unions {
            expand_group_by_all(peer);
        }
        return;
    }
    let mut groups: Vec<Expr> = Vec::new();
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item
            && !aggregate::contains_aggregate(expr)
        {
            groups.push(expr.clone());
        }
    }
    s.group_by = Some(groups);
    s.group_by_all = false;
    for (_, peer) in &mut s.unions {
        expand_group_by_all(peer);
    }
}

fn resolve_order_by_position(s: &mut SelectStatement) {
    // v6.4.0 — iterate every ORDER BY key. Position references
    // (`ORDER BY 2`) bind to the 1-based projection index;
    // identifier references that match a SELECT-list alias bind to
    // the projected expression (Step 4 of L3a).
    for order in &mut s.order_by {
        match &order.expr {
            Expr::Literal(Literal::Integer(n)) if *n >= 1 => {
                if let Ok(idx_one_based) = usize::try_from(*n) {
                    let idx = idx_one_based - 1;
                    if idx < s.items.len()
                        && let SelectItem::Expr { expr, .. } = &s.items[idx]
                    {
                        order.expr = expr.clone();
                    }
                }
            }
            Expr::Column(c) if c.qualifier.is_none() => {
                // Alias-in-ORDER-BY lookup.
                for item in &s.items {
                    if let SelectItem::Expr {
                        expr,
                        alias: Some(a),
                    } = item
                        && a == &c.name
                    {
                        order.expr = expr.clone();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    for (_, peer) in &mut s.unions {
        resolve_order_by_position(peer);
    }
}

/// Sort `tagged` by `f64` key, reversing the comparator under DESC.
/// Used by the UNION ORDER BY path; per-block paths inline the same
/// comparator because they already hold `&OrderBy` directly.
/// v3.1.1: partial-sort helper. When `keep` (= offset + limit) is
/// strictly less than `tagged.len()`, run `select_nth_unstable_by` to
/// partition the prefix in O(n), then sort just that prefix in O(k
/// log k). Total O(n + k log k), vs O(n log n) for a full sort. The
/// caller decides what `keep` is; passing `None` (no LIMIT) keeps the
/// full-sort behaviour.
///
/// `tagged` holds `(Option<f64>, Row)` (the SELECT path) — `None` keys
/// sort last in ascending order, mirroring NULL-sorts-last in SQL.
fn partial_sort_tagged(tagged: &mut Vec<(Vec<f64>, Row)>, keep: Option<usize>, descs: &[bool]) {
    let cmp = |a: &(Vec<f64>, Row), b: &(Vec<f64>, Row)| cmp_multi_key(&a.0, &b.0, descs);
    match keep {
        Some(k) if k < tagged.len() && k > 0 => {
            let pivot = k - 1;
            tagged.select_nth_unstable_by(pivot, cmp);
            tagged[..k].sort_by(cmp);
            tagged.truncate(k);
        }
        _ => {
            tagged.sort_by(cmp);
        }
    }
}

fn sort_by_keys(tagged: &mut [(Vec<f64>, Row)], descs: &[bool]) {
    tagged.sort_by(|a, b| cmp_multi_key(&a.0, &b.0, descs));
}

/// v6.4.0 — multi-key ORDER BY comparator. Each key's per-key DESC
/// flag is honored independently. NULL is encoded as `f64::INFINITY`
/// so it sorts last in ASC and first in DESC (matches PG default).
fn cmp_multi_key(a: &[f64], b: &[f64], descs: &[bool]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (i, (ka, kb)) in a.iter().zip(b.iter()).enumerate() {
        let ord = ka.partial_cmp(kb).unwrap_or(Ordering::Equal);
        let ord = if descs.get(i).copied().unwrap_or(false) {
            ord.reverse()
        } else {
            ord
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// v6.4.0 — eval every ORDER BY expression for a row and pack the
/// resulting keys into a `Vec<f64>`. NULL → `f64::INFINITY`.
fn build_order_keys(
    order_by: &[OrderBy],
    row: &Row,
    ctx: &EvalContext,
) -> Result<Vec<f64>, EngineError> {
    let mut keys = Vec::with_capacity(order_by.len());
    for o in order_by {
        let v = eval::eval_expr(&o.expr, row, ctx)?;
        // v7.24 (round-16 A) — explicit NULLS FIRST/LAST. The f64
        // packing sorts ascending THEN applies the per-key DESC
        // reverse, so a NULL must land at +INF exactly when the
        // effective placement agrees with the reverse direction:
        // nf == desc → +INF (ASC default last / DESC default
        // first), nf != desc → -INF (the explicit flips).
        if matches!(v, Value::Null) {
            let nf = o.nulls_first.unwrap_or(o.desc);
            keys.push(if nf == o.desc {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            });
        } else {
            keys.push(value_to_order_key(&v)?);
        }
    }
    Ok(keys)
}

/// Drop the first `offset` rows then truncate to `limit`. PG / `MySQL`
/// agree: OFFSET applies *after* ORDER BY but *before* LIMIT (so
/// `LIMIT 10 OFFSET 5` keeps rows 6..=15).
fn apply_offset_and_limit(rows: &mut Vec<Row>, offset: Option<u32>, limit: Option<u32>) {
    if let Some(off) = offset {
        let off = off as usize;
        if off >= rows.len() {
            rows.clear();
        } else {
            rows.drain(..off);
        }
    }
    if let Some(n) = limit {
        rows.truncate(n as usize);
    }
}

/// v7.17.0 Phase 3.P0-49 — offset + limit applied to a tagged
/// `(order_keys, row)` sequence, with optional SQL:2008 `WITH
/// TIES` extension. When `with_ties` is set, the truncated tail
/// is extended through every subsequent row whose order keys
/// equal the last-kept row's keys (so a "top 3 by score" with
/// WITH TIES emits row 4 too when row 4 ties row 3 on `score`).
///
/// The order-key vector is the per-row sort key the caller already
/// computed via `build_order_keys`; equal-key detection therefore
/// matches the sort comparator exactly.
fn apply_offset_and_limit_tagged(
    tagged: &mut Vec<(Vec<f64>, Row)>,
    offset: Option<u32>,
    limit: Option<u32>,
    with_ties: bool,
) {
    if let Some(off) = offset {
        let off = off as usize;
        if off >= tagged.len() {
            tagged.clear();
        } else {
            tagged.drain(..off);
        }
    }
    if let Some(n) = limit {
        let n = n as usize;
        if with_ties && n > 0 && n < tagged.len() {
            let cutoff_key = tagged[n - 1].0.clone();
            let mut end = n;
            while end < tagged.len() && tagged[end].0 == cutoff_key {
                end += 1;
            }
            tagged.truncate(end);
        } else {
            tagged.truncate(n);
        }
    }
}

/// v7.17.0 Phase 3.P0-49 — PG-canonical: `FETCH FIRST <n> ROWS
/// WITH TIES` requires an `ORDER BY`. Without one, there's no
/// way to identify "ties" deterministically, so PG errors at
/// plan time. SPG mirrors that surface so the same DDL / app
/// behaviour holds on cutover.
fn check_with_ties_requires_order_by(stmt: &SelectStatement) -> Result<(), EngineError> {
    if stmt.limit_with_ties && stmt.order_by.is_empty() {
        return Err(EngineError::Unsupported(alloc::string::String::from(
            "FETCH FIRST … ROWS WITH TIES requires an ORDER BY clause",
        )));
    }
    Ok(())
}

/// v7.6.1 — resolve a parser-level `ForeignKeyConstraint` (column
/// names + parent table name) into the storage-layer shape (column
/// indices + same parent table). Validates everything the engine
/// needs to know about the FK at CREATE TABLE time:
///
///   - parent table exists (catalog lookup, unless self-referencing)
///   - parent columns exist on the parent table
///   - parent column list matches the local arity (defaults to the
///     parent's primary index column when omitted)
///   - parent columns are covered by a `BTree` UNIQUE-class index
///     (SPG's stand-in for `PRIMARY KEY`/`UNIQUE`) — required so
///     the v7.6.2 INSERT path can do an O(log n) parent lookup
///   - local columns exist on the table being created
fn resolve_foreign_key(
    local_table_name: &str,
    local_cols: &[ColumnSchema],
    fk: spg_sql::ast::ForeignKeyConstraint,
    catalog: &Catalog,
) -> Result<spg_storage::ForeignKeyConstraint, EngineError> {
    // Resolve local columns.
    let mut local_columns = Vec::with_capacity(fk.columns.len());
    for name in &fk.columns {
        let pos = local_cols
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "FOREIGN KEY references unknown local column {name:?}"
                ))
            })?;
        local_columns.push(pos);
    }
    // Self-referencing FK: parent table is the one we're creating.
    // The parent column resolution uses the local column list since
    // the catalog doesn't have this table yet.
    let is_self_ref = fk.parent_table == local_table_name;
    let (parent_cols_for_lookup, parent_table_str): (&[ColumnSchema], &str) = if is_self_ref {
        (local_cols, local_table_name)
    } else {
        let parent_table = catalog.get(&fk.parent_table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: fk.parent_table.clone(),
            })
        })?;
        (
            parent_table.schema().columns.as_slice(),
            fk.parent_table.as_str(),
        )
    };
    // Resolve parent column names → positions. If the FK omitted the
    // parent column list, fall back to the parent's primary index
    // column (single-column only — composite default is rejected
    // because there's no unambiguous "PK" in SPG's index list).
    let parent_columns: Vec<usize> = if fk.parent_columns.is_empty() {
        if fk.columns.len() != 1 {
            return Err(EngineError::Unsupported(
                "composite FOREIGN KEY without explicit parent column list is not supported \
                 — list the parent columns explicitly"
                    .into(),
            ));
        }
        // Find a single BTree index on the parent and use its column.
        let pos = pick_pk_index_column(catalog, parent_table_str, is_self_ref, local_cols)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "parent table {parent_table_str:?} has no PRIMARY-key / UNIQUE BTree index \
                     to default the FOREIGN KEY against"
                ))
            })?;
        alloc::vec![pos]
    } else {
        let mut out = Vec::with_capacity(fk.parent_columns.len());
        for name in &fk.parent_columns {
            let pos = parent_cols_for_lookup
                .iter()
                .position(|c| c.name == *name)
                .ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY references unknown parent column \
                         {name:?} on table {parent_table_str:?}"
                    ))
                })?;
            out.push(pos);
        }
        out
    };
    if parent_columns.len() != local_columns.len() {
        return Err(EngineError::Unsupported(alloc::format!(
            "FOREIGN KEY arity mismatch: {} local columns vs {} parent columns",
            local_columns.len(),
            parent_columns.len()
        )));
    }
    // For non-self-referencing FKs, verify the parent column set is
    // covered by a BTree index. SPG doesn't have a `PRIMARY KEY`
    // declaration; the convention is "the parent column for FK
    // purposes must have a BTree index" — which the user creates via
    // `CREATE INDEX ... USING btree (col)` (the default). We accept
    // any single-column BTree index that covers a parent column;
    // composite parent column lists require an index whose `column_position`
    // matches the first parent column (multi-column BTree indices
    // are not in the v7.x roadmap).
    if !is_self_ref {
        let parent_table = catalog.get(&fk.parent_table).expect("checked above");
        let primary_parent_col = parent_columns[0];
        let has_btree = parent_table
            .schema()
            .columns
            .get(primary_parent_col)
            .is_some()
            && parent_table.indices().iter().any(|idx| {
                matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                    && idx.column_position == primary_parent_col
                    && idx.partial_predicate.is_none()
            });
        if !has_btree {
            return Err(EngineError::Unsupported(alloc::format!(
                "FOREIGN KEY parent column on {:?} is not covered by an unconditional BTree \
                 index — create one with `CREATE INDEX ... ON {} ({})` first",
                parent_table_str,
                parent_table_str,
                parent_table.schema().columns[primary_parent_col].name,
            )));
        }
    }
    let on_delete = fk_action_sql_to_storage(fk.on_delete);
    let on_update = fk_action_sql_to_storage(fk.on_update);
    Ok(spg_storage::ForeignKeyConstraint {
        name: fk.name,
        local_columns,
        parent_table: fk.parent_table,
        parent_columns,
        on_delete,
        on_update,
    })
}

/// v7.6.1 — pick a sentinel "primary key" column from the parent
/// table when the FK didn't name parent columns. Picks the first
/// single-column unconditional BTree index — that's the closest
/// thing SPG has to a PRIMARY KEY today. Self-referencing FKs use
/// `local_cols` as the column source.
fn pick_pk_index_column(
    catalog: &Catalog,
    parent_name: &str,
    is_self_ref: bool,
    local_cols: &[ColumnSchema],
) -> Option<usize> {
    if is_self_ref {
        // Self-ref FK omitted parent columns: pick column 0 by
        // convention (no catalog entry yet). Engine will widen this
        // when v7.6.7 lands; v7.6.1 only handles the explicit form.
        let _ = local_cols;
        return Some(0);
    }
    let parent = catalog.get(parent_name)?;
    parent.indices().iter().find_map(|idx| {
        if matches!(idx.kind, spg_storage::IndexKind::BTree(_))
            && idx.partial_predicate.is_none()
            && idx.included_columns.is_empty()
            && idx.expression.is_none()
        {
            Some(idx.column_position)
        } else {
            None
        }
    })
}

/// v7.9.8 / v7.9.10 — resolve the column positions that
/// identify a conflict for ON CONFLICT. Returns a Vec of
/// column positions (1 element for single-column form, N for
/// composite). When the user wrote bare `ON CONFLICT DO …`,
/// falls back to the table's first unconditional BTree index
/// (always single-column today).
/// Returns the conflict-key column positions plus whether the
/// matched constraint declares NULLS NOT DISTINCT (v7.29 — a NULL
/// in the key only rules out a conflict under the default
/// NULLS DISTINCT semantics).
fn resolve_on_conflict_columns(
    catalog: &Catalog,
    table_name: &str,
    target: &[String],
) -> Result<(Vec<usize>, bool), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    if target.is_empty() {
        // v7.13.2 — mailrs round-6 S5 follow-up. Composite UNIQUE
        // constraints carry a multi-column tuple; the prior code
        // path picked only the leading column of the first BTree
        // index, which caused `ON CONFLICT DO NOTHING` to dedup
        // by leading column alone (3 rows with same group_id but
        // different permission collapsed to 1). PG semantics use
        // the full tuple. Prefer a UniquenessConstraint's full
        // column list when one exists; fall back to the leading
        // BTree column for legacy single-column UNIQUE.
        if let Some(uc) = table.schema().uniqueness_constraints.first() {
            return Ok((uc.columns.clone(), uc.nulls_not_distinct));
        }
        let pos = table
            .indices()
            .iter()
            .find_map(|idx| {
                if matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                    && idx.partial_predicate.is_none()
                    && idx.included_columns.is_empty()
                    && idx.expression.is_none()
                {
                    Some(idx.column_position)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ON CONFLICT without target requires a UNIQUE BTree index on {table_name:?}"
                ))
            })?;
        return Ok((alloc::vec![pos], false));
    }
    let mut out = Vec::with_capacity(target.len());
    for name in target {
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ON CONFLICT target column {name:?} not found on {table_name:?}"
                ))
            })?;
        out.push(pos);
    }
    // An explicit target matching a UNIQUE constraint inherits its
    // NULLS [NOT] DISTINCT declaration.
    let mut sorted = out.clone();
    sorted.sort_unstable();
    let nnd = table.schema().uniqueness_constraints.iter().any(|uc| {
        let mut u = uc.columns.clone();
        u.sort_unstable();
        u == sorted && uc.nulls_not_distinct
    });
    Ok((out, nnd))
}

/// v7.9.8 — check whether the BTree index on `column_pos` of
/// `table_name` already has a row with this key.
fn on_conflict_key_exists(
    catalog: &Catalog,
    table_name: &str,
    column_pos: usize,
    key: &Value,
) -> bool {
    let Some(table) = catalog.get(table_name) else {
        return false;
    };
    let Some(idx_key) = spg_storage::IndexKey::from_value(key) else {
        return false;
    };
    table.indices().iter().any(|idx| {
        matches!(idx.kind, spg_storage::IndexKind::BTree(_))
            && idx.column_position == column_pos
            && idx.partial_predicate.is_none()
            && !idx.lookup_eq(&idx_key).is_empty()
    })
}

/// v7.9.9 / v7.9.10 — look up an existing row's position by
/// matching all `column_positions` against the incoming `key`
/// tuple. Single-column shape (one column) reduces to the
/// canonical PK lookup; composite shapes scan linearly until
/// every position matches.
fn lookup_row_position_by_keys(
    catalog: &Catalog,
    table_name: &str,
    column_positions: &[usize],
    key: &[&Value],
) -> Option<usize> {
    let table = catalog.get(table_name)?;
    table.rows().iter().position(|r| {
        column_positions
            .iter()
            .enumerate()
            .all(|(i, &pos)| r.values.get(pos) == Some(key[i]))
    })
}

/// v7.9.10 — does the table already contain a row whose
/// `column_positions` tuple equals `key`? Single-column shape
/// uses the existing BTree fast path; composite shapes fall
/// back to a row scan.
fn on_conflict_keys_exist(
    catalog: &Catalog,
    table_name: &str,
    column_positions: &[usize],
    key: &[&Value],
) -> bool {
    if column_positions.len() == 1 {
        return on_conflict_key_exists(catalog, table_name, column_positions[0], key[0]);
    }
    let Some(table) = catalog.get(table_name) else {
        return false;
    };
    table.rows().iter().any(|r| {
        column_positions
            .iter()
            .enumerate()
            .all(|(i, &pos)| r.values.get(pos) == Some(key[i]))
    })
}

/// v7.9.9 — apply ON CONFLICT DO UPDATE SET assignments to an
/// existing row.
///
/// `incoming` is the rejected INSERT row (used to resolve
/// `EXCLUDED.col` references in the assignment exprs);
/// `target_pos` is the position of the existing row in the table.
/// Each assignment substitutes `EXCLUDED.col` with the matching
/// incoming value, evaluates the resulting expression against
/// the existing row, and writes the new value into the
/// corresponding column of the returned `Vec<Value>`. If
/// `where_` evaluates falsy, returns Ok(None) — PG behaviour:
/// the conflicting row is silently kept unchanged.
fn apply_on_conflict_assignments(
    catalog: &Catalog,
    table_name: &str,
    target_pos: usize,
    incoming: &[Value],
    assignments: &[(String, Expr)],
    where_: Option<&Expr>,
) -> Result<Option<Vec<Value>>, EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema_cols = table.schema().columns.clone();
    let existing = table
        .rows()
        .get(target_pos)
        .ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "ON CONFLICT DO UPDATE: row position {target_pos} out of bounds on {table_name:?}"
            ))
        })?
        .clone();
    let ctx = eval::EvalContext::new(&schema_cols, Some(table_name));
    // Optional WHERE filter on the conflict row.
    if let Some(w) = where_ {
        let pred = w.clone();
        let pred = substitute_excluded_refs(pred, &schema_cols, incoming);
        let v = eval::eval_expr(&pred, &existing, &ctx)?;
        if !matches!(v, Value::Bool(true)) {
            return Ok(None);
        }
    }
    let mut new_values = existing.values.clone();
    for (col_name, expr) in assignments {
        let target_idx = schema_cols
            .iter()
            .position(|c| c.name == *col_name)
            .ok_or_else(|| {
                EngineError::Eval(EvalError::ColumnNotFound {
                    name: col_name.clone(),
                })
            })?;
        let sub = substitute_excluded_refs(expr.clone(), &schema_cols, incoming);
        let v = eval::eval_expr(&sub, &existing, &ctx)?;
        let coerced = coerce_value(v, schema_cols[target_idx].ty, col_name, target_idx)?;
        check_unsigned_range(&coerced, &schema_cols[target_idx], target_idx)?;
        new_values[target_idx] = coerced;
    }
    Ok(Some(new_values))
}

/// v7.9.9 — walk an `Expr` tree replacing any `Column { qualifier:
/// "EXCLUDED", name }` reference with a `Literal` of the matching
/// value from the incoming-row vec. Resolution against the
/// child-table column list (by name).
fn substitute_excluded_refs(expr: Expr, schema_cols: &[ColumnSchema], incoming: &[Value]) -> Expr {
    use spg_sql::ast::ColumnName;
    match expr {
        Expr::Column(ColumnName { qualifier, name })
            if qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case("excluded")) =>
        {
            let pos = schema_cols.iter().position(|c| c.name == name);
            match pos {
                Some(p) => {
                    let v = incoming.get(p).cloned().unwrap_or(Value::Null);
                    value_to_literal_expr(v)
                        .unwrap_or_else(|_| Expr::Literal(spg_sql::ast::Literal::Null))
                }
                None => Expr::Column(ColumnName { qualifier, name }),
            }
        }
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op,
            lhs: Box::new(substitute_excluded_refs(*lhs, schema_cols, incoming)),
            rhs: Box::new(substitute_excluded_refs(*rhs, schema_cols, incoming)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|a| substitute_excluded_refs(a, schema_cols, incoming))
                .collect(),
        },
        other => other,
    }
}

/// v7.6.2 / v7.6.7 — INSERT-side FK enforcement. For every row
/// about to be inserted into `child_table`, every FK declared on
/// that table is checked: the row's FK columns must either be
/// NULL (SQL spec skip) or match an existing parent row via the
/// parent's BTree PK / UNIQUE index.
///
/// Returns `EngineError::Unsupported` with a `FOREIGN KEY violation`
/// payload on first failure.
///
/// **Self-referencing FKs (v7.6.7 widening):** when `fk.parent_table
/// == child_table`, the parent rows visible to this check are
///  (a) rows already committed to the table, plus
///  (b) earlier rows from the *same* `rows` batch.
/// This makes `INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2)`
/// work in a single statement — common pattern for bulk-loading
/// hierarchies.
/// v7.9.19 — enforce table-level UNIQUE / PRIMARY KEY tuple
/// constraints at INSERT time. For each constraint declared on
/// the target table, check that no existing row + no earlier row
/// in the same batch has the same full-column tuple. NULL in
/// any column lifts the row out of the check (SQL spec: NULL
/// ≠ NULL for uniqueness). mailrs G1 + G6.
fn enforce_uniqueness_inserts(
    catalog: &Catalog,
    child_table: &str,
    constraints: &[spg_storage::UniquenessConstraint],
    rows: &[Vec<Value>],
) -> Result<(), EngineError> {
    if constraints.is_empty() {
        return Ok(());
    }
    let table = catalog.get(child_table).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: child_table.into(),
        })
    })?;
    let schema = table.schema();
    // v7.29 (mailrs round-23b) — set-based: ONE O(table) pass folds
    // existing keys into a hash set, then each batch row is a probe
    // + insert. The previous shape scanned the WHOLE table per
    // inserted row (and earlier batch rows per row), which made
    // bulk import O(n²) — a 104 MB dump extrapolated to ~1 hour
    // (PG: 2 min). Collation folding (Phase 3.P0-45) and
    // NULLS [NOT] DISTINCT semantics are unchanged: keys fold via
    // collated_key_cell before encoding, NULL-bearing keys skip the
    // set unless nulls_not_distinct.
    for uc in constraints {
        let fold_key = |values: &[Value]| -> Vec<Value> {
            uc.columns
                .iter()
                .map(|&i| {
                    let v = values.get(i).cloned().unwrap_or(Value::Null);
                    collated_key_cell(&v, i, schema)
                })
                .collect()
        };
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for prow in table.rows() {
            let key = fold_key(&prow.values);
            if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                continue;
            }
            seen.insert(aggregate::encode_key(&key));
        }
        for (batch_idx, row_values) in rows.iter().enumerate() {
            let key = fold_key(row_values);
            if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                continue;
            }
            if !seen.insert(aggregate::encode_key(&key)) {
                let kind = if uc.is_primary_key {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                let col_names: Vec<String> = uc
                    .columns
                    .iter()
                    .map(|&i| table.schema().columns[i].name.clone())
                    .collect();
                return Err(EngineError::Unsupported(alloc::format!(
                    "{kind} violation on {child_table:?} columns {col_names:?}: \
                     row #{batch_idx} duplicates an existing key"
                )));
            }
        }
    }
    Ok(())
}

/// v7.17.0 Phase 3.P0-45 — return a key cell folded by its column's
/// declared `Collation`. For `CaseInsensitive`, fold Text payloads to
/// ASCII lowercase (matches Phase 2.5's `*_ci` semantics: ASCII case-
/// fold only, non-ASCII bytes stay byte-wise). For `Binary` or non-Text
/// values, the cell passes through unchanged. The caller compares the
/// folded values with `==`.
fn collated_key_cell(
    v: &spg_storage::Value,
    column_position: usize,
    schema: &spg_storage::TableSchema,
) -> spg_storage::Value {
    match (v, schema.columns.get(column_position).map(|c| c.collation)) {
        (spg_storage::Value::Text(s), Some(spg_storage::Collation::CaseInsensitive)) => {
            spg_storage::Value::Text(s.to_ascii_lowercase())
        }
        _ => v.clone(),
    }
}

/// v7.9.29 — `true` iff `v` counts as a truthy SQL value for a
/// WHERE-style predicate. NULL → false (three-valued logic
/// collapses to "skip this row" for index inclusion). Numeric
/// non-zero, BIGINT non-zero, TINYINT non-zero, BOOLEAN true → true.
/// Everything else (strings, vectors, JSON, …) is not a valid
/// predicate result and surfaces as `false` so a malformed
/// predicate degrades to "row not in index" rather than panicking.
fn predicate_truthy(v: &spg_storage::Value) -> bool {
    use spg_storage::Value as V;
    match v {
        V::Bool(b) => *b,
        V::Int(n) => *n != 0,
        V::BigInt(n) => *n != 0,
        V::SmallInt(n) => *n != 0,
        _ => false,
    }
}

/// v7.9.29 — at CREATE UNIQUE INDEX time, scan the table's
/// committed rows for pre-existing duplicates. If any pair of rows
/// matches the predicate AND has the same index key, refuse to
/// create the index so the user fixes the data before retrying.
fn check_existing_unique_violation(
    idx: &spg_storage::Index,
    schema: &spg_storage::TableSchema,
    rows: &[spg_storage::Row],
) -> Result<(), EngineError> {
    let predicate_expr = match idx.partial_predicate.as_deref() {
        Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "stored partial predicate {s:?} failed to re-parse: {e:?}"
            ))
        })?),
        None => None,
    };
    let ctx = eval::EvalContext::new(&schema.columns, None);
    let key_positions = unique_key_positions(idx);
    let mut seen: alloc::vec::Vec<alloc::vec::Vec<spg_storage::Value>> = alloc::vec::Vec::new();
    for row in rows {
        if let Some(expr) = &predicate_expr {
            let v = eval::eval_expr(expr, row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "evaluating UNIQUE INDEX predicate against existing row: {e:?}"
                ))
            })?;
            if !predicate_truthy(&v) {
                continue;
            }
        }
        let key: alloc::vec::Vec<spg_storage::Value> = key_positions
            .iter()
            .map(|&p| {
                let v = row
                    .values
                    .get(p)
                    .cloned()
                    .unwrap_or(spg_storage::Value::Null);
                collated_key_cell(&v, p, schema)
            })
            .collect();
        if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
            continue;
        }
        if seen.iter().any(|other| *other == key) {
            return Err(EngineError::Unsupported(alloc::format!(
                "CREATE UNIQUE INDEX {:?}: existing rows already violate the constraint",
                idx.name
            )));
        }
        seen.push(key);
    }
    Ok(())
}

/// v7.9.29 — full key tuple for a UNIQUE INDEX (leading +
/// extra positions). For single-column indexes this is just
/// `[column_position]`.
fn unique_key_positions(idx: &spg_storage::Index) -> alloc::vec::Vec<usize> {
    let mut out = alloc::vec::Vec::with_capacity(1 + idx.extra_column_positions.len());
    out.push(idx.column_position);
    out.extend_from_slice(&idx.extra_column_positions);
    out
}

/// v7.9.29 — at INSERT time, walk every `is_unique` index on the
/// target table. For each, eval the index's optional predicate
/// against (a) the candidate row and (b) every committed row plus
/// earlier batch rows; only rows where the predicate is truthy
/// participate. A duplicate key among predicate-matching rows is a
/// uniqueness violation. NULL keys lift the row out of the check
/// (matching PG's "UNIQUE allows multiple NULLs" semantics).
fn enforce_unique_index_inserts(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<spg_storage::Value>],
) -> Result<(), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema = table.schema();
    let ctx = eval::EvalContext::new(&schema.columns, None);
    for idx in table.indices() {
        if !idx.is_unique {
            continue;
        }
        // Re-parse the predicate once per index per batch.
        let predicate_expr = match idx.partial_predicate.as_deref() {
            Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} predicate {s:?} failed to re-parse: {e:?}",
                    idx.name
                ))
            })?),
            None => None,
        };
        let key_positions = unique_key_positions(idx);
        let key_of = |values: &[spg_storage::Value]| -> alloc::vec::Vec<spg_storage::Value> {
            key_positions
                .iter()
                .map(|&p| {
                    let v = values.get(p).cloned().unwrap_or(spg_storage::Value::Null);
                    collated_key_cell(&v, p, schema)
                })
                .collect()
        };
        let participates = |values: &[spg_storage::Value]| -> Result<bool, EngineError> {
            let Some(expr) = &predicate_expr else {
                return Ok(true);
            };
            let tmp_row = spg_storage::Row {
                values: values.to_vec(),
            };
            let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} predicate eval: {e:?}",
                    idx.name
                ))
            })?;
            Ok(predicate_truthy(&v))
        };
        // v7.29 (mailrs round-23b) — set-based: one O(table) pass
        // (predicate evaluated once per existing row instead of once
        // per row PAIR), then probe per batch row. The previous
        // nested scans made bulk import O(n²).
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for prow in table.rows() {
            if !participates(&prow.values)? {
                continue;
            }
            let key = key_of(&prow.values);
            if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
                continue;
            }
            seen.insert(aggregate::encode_key(&key));
        }
        for (batch_idx, row_values) in rows.iter().enumerate() {
            if !participates(row_values)? {
                continue;
            }
            let key = key_of(row_values);
            if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
                continue;
            }
            if !seen.insert(aggregate::encode_key(&key)) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} violation on {table_name:?}: \
                     row #{batch_idx} duplicates an existing key",
                    idx.name
                )));
            }
        }
    }
    Ok(())
}

/// v7.13.0 — `UPDATE OF cols` filter helper (mailrs round-5 G7).
/// Returns `true` when at least one of `filter_cols` has a
/// different value in `new_row` vs `old_row`. Column lookup is
/// case-insensitive against `schema_cols`; unknown filter columns
/// are treated as "not changed" (the trigger therefore won't
/// fire on them — surfacing a parse-time error would be too
/// strict for catalog reloads where the schema may have drifted).
fn any_column_changed(
    filter_cols: &[String],
    schema_cols: &[ColumnSchema],
    old_row: &Row,
    new_row: &Row,
) -> bool {
    for col_name in filter_cols {
        let Some(pos) = schema_cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col_name))
        else {
            continue;
        };
        let old_v = old_row.values.get(pos);
        let new_v = new_row.values.get(pos);
        if old_v != new_v {
            return true;
        }
    }
    false
}

/// v7.13.0 — evaluate every CHECK predicate on the schema against
/// each candidate row. Mirrors PG semantics: a `false` result
/// rejects the mutation; a NULL result *passes* (CHECK rejects
/// only on definite-false, not on unknown). mailrs round-5 G3.
fn enforce_check_constraints(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<spg_storage::Value>],
) -> Result<(), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema = table.schema();
    // v7.17.0 Phase 1.5 — domain-level CHECKs are enforced in
    // parallel with table-level CHECKs. Collect both lists up
    // front; if neither exists we early-out.
    let mut domain_checks_per_col: alloc::vec::Vec<(usize, alloc::vec::Vec<Expr>)> =
        alloc::vec::Vec::new();
    for (idx, col) in schema.columns.iter().enumerate() {
        let Some(dname) = &col.user_domain_type else {
            continue;
        };
        let Some(dom) = catalog.domain_types().get(dname) else {
            continue;
        };
        let mut parsed_for_col: alloc::vec::Vec<Expr> =
            alloc::vec::Vec::with_capacity(dom.checks.len());
        for src in &dom.checks {
            let expr = spg_sql::parser::parse_expression(src).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "DOMAIN {dname:?} CHECK ({src:?}) on column {:?}: re-parse failed: {e:?}",
                    col.name
                ))
            })?;
            parsed_for_col.push(expr);
        }
        if !parsed_for_col.is_empty() {
            domain_checks_per_col.push((idx, parsed_for_col));
        }
    }
    if schema.checks.is_empty() && domain_checks_per_col.is_empty() {
        return Ok(());
    }
    let ctx = eval::EvalContext::new(&schema.columns, None);
    let mut parsed: alloc::vec::Vec<(usize, Expr)> = alloc::vec::Vec::new();
    for (i, src) in schema.checks.iter().enumerate() {
        let expr = spg_sql::parser::parse_expression(src).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "CHECK constraint #{i} on {table_name:?} ({src:?}) failed to re-parse: {e:?}"
            ))
        })?;
        parsed.push((i, expr));
    }
    for (batch_idx, row_values) in rows.iter().enumerate() {
        let tmp_row = spg_storage::Row {
            values: row_values.clone(),
        };
        for (i, expr) in &parsed {
            let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "CHECK constraint #{i} on {table_name:?} eval at row #{batch_idx}: {e:?}"
                ))
            })?;
            // PG: NULL passes (CHECK rejects on definite-false only).
            if matches!(v, spg_storage::Value::Bool(false)) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CHECK constraint violation on {table_name:?} (row #{batch_idx}): {:?}",
                    schema.checks[*i]
                )));
            }
        }
        // v7.17.0 Phase 1.5 — domain-level CHECKs. Each CHECK
        // expression references VALUE as a column-name; we
        // substitute the per-row cell into the eval context by
        // synthesising a single-column row of just that value
        // under a temporary `value` column schema.
        for (col_idx, checks) in &domain_checks_per_col {
            let cell = row_values
                .get(*col_idx)
                .cloned()
                .unwrap_or(spg_storage::Value::Null);
            let synth_cols = alloc::vec![spg_storage::ColumnSchema::new(
                "value",
                schema.columns[*col_idx].ty,
                schema.columns[*col_idx].nullable,
            )];
            let synth_ctx = eval::EvalContext::new(&synth_cols, None);
            let synth_row = spg_storage::Row {
                values: alloc::vec![cell],
            };
            for (ci, expr) in checks.iter().enumerate() {
                let v = eval::eval_expr(expr, &synth_row, &synth_ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "DOMAIN CHECK #{ci} on column {:?} eval at row #{batch_idx}: {e:?}",
                        schema.columns[*col_idx].name
                    ))
                })?;
                if matches!(v, spg_storage::Value::Bool(false)) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "DOMAIN CHECK violation on column {:?} (row #{batch_idx})",
                        schema.columns[*col_idx].name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn enforce_fk_inserts(
    catalog: &Catalog,
    child_table: &str,
    fks: &[spg_storage::ForeignKeyConstraint],
    rows: &[Vec<Value>],
) -> Result<(), EngineError> {
    for fk in fks {
        let parent_is_self = fk.parent_table == child_table;
        let parent = if parent_is_self {
            // Self-ref: read the current state of the same table.
            // The mut borrow on child has been dropped by the caller.
            catalog.get(child_table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: child_table.into(),
                })
            })?
        } else {
            catalog.get(&fk.parent_table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: fk.parent_table.clone(),
                })
            })?
        };
        for (batch_idx, row_values) in rows.iter().enumerate() {
            // Single-column FK fast path: try the parent's BTree
            // index for an O(log n) lookup. Composite FKs fall back
            // to a parent-row scan.
            if fk.local_columns.len() == 1 {
                let v = &row_values[fk.local_columns[0]];
                if matches!(v, Value::Null) {
                    continue;
                }
                let parent_col = fk.parent_columns[0];
                let key = spg_storage::IndexKey::from_value(v).ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY column value of type {:?} is not index-eligible",
                        v.data_type()
                    ))
                })?;
                let present_committed = parent.indices().iter().any(|idx| {
                    matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                        && idx.column_position == parent_col
                        && idx.partial_predicate.is_none()
                        && !idx.lookup_eq(&key).is_empty()
                });
                // v7.6.7 self-ref widening: also accept a match
                // against earlier rows in this same batch when the
                // FK points at the table being inserted into.
                let present_in_batch = parent_is_self
                    && rows[..batch_idx]
                        .iter()
                        .any(|earlier| earlier.get(parent_col) == Some(v));
                if !(present_committed || present_in_batch) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY violation: no parent row in {:?} where {} = {:?}",
                        fk.parent_table,
                        parent
                            .schema()
                            .columns
                            .get(parent_col)
                            .map_or("?", |c| c.name.as_str()),
                        v,
                    )));
                }
            } else {
                // Composite FK: scan parent rows. v7.6.7 also
                // accepts a match against earlier rows in the same
                // batch (self-ref bulk-loading of hierarchies).
                if fk
                    .local_columns
                    .iter()
                    .all(|&i| matches!(row_values.get(i), Some(Value::Null)))
                {
                    continue;
                }
                let local: Vec<&Value> = fk.local_columns.iter().map(|&i| &row_values[i]).collect();
                let parent_match_committed = parent.rows().iter().any(|prow| {
                    fk.parent_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &pi)| prow.values.get(pi) == Some(local[i]))
                });
                let parent_match_in_batch = parent_is_self
                    && rows[..batch_idx].iter().any(|earlier| {
                        fk.parent_columns
                            .iter()
                            .enumerate()
                            .all(|(i, &pi)| earlier.get(pi) == Some(local[i]))
                    });
                if !(parent_match_committed || parent_match_in_batch) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY violation: no parent row in {:?} matching composite key",
                        fk.parent_table,
                    )));
                }
            }
        }
    }
    Ok(())
}

/// v7.6.4 / v7.6.5 — one step of the FK action plan computed for a
/// DELETE on a parent. The plan is a list of these steps, stacked
/// across the FK graph by `plan_fk_parent_deletions`.
#[derive(Debug, Clone)]
struct FkChildStep {
    child_table: String,
    action: FkChildAction,
}

#[derive(Debug, Clone)]
enum FkChildAction {
    /// CASCADE — remove these rows. Sorted, deduplicated positions.
    Delete { positions: Vec<usize> },
    /// SET NULL — for each (row, column) in the flat list, write
    /// NULL into that child cell. Multiple FKs on the same row may
    /// produce overlapping entries (deduped at plan time).
    SetNull {
        positions: Vec<usize>,
        columns: Vec<usize>,
    },
    /// SET DEFAULT — same shape as SetNull but writes the column's
    /// declared DEFAULT value (resolved at plan time). Columns
    /// without a DEFAULT raise an error during planning.
    SetDefault {
        positions: Vec<usize>,
        columns: Vec<usize>,
        defaults: Vec<Value>,
    },
}

/// v7.6.3 → v7.6.5 — plan FK fallout for a DELETE on a parent table.
///
/// Walks every table in the catalog looking for FKs whose
/// `parent_table` is `parent_table_name`. For each such FK + each
/// to-be-deleted parent row:
///
///   - RESTRICT / NoAction → error, no plan returned
///   - CASCADE → child rows get scheduled for deletion; recursive
///   - SetNull → child FK column(s) scheduled to be NULL-ed.
///     Verified NULL-able at plan time.
///   - SetDefault → child FK column(s) scheduled to be reset to
///     their declared DEFAULT. Columns without a DEFAULT raise.
///
/// SET NULL / SET DEFAULT do NOT cascade further — the child row
/// stays; only one of its columns mutates.
fn plan_fk_parent_deletions(
    catalog: &Catalog,
    parent_table_name: &str,
    to_delete_positions: &[usize],
    to_delete_rows: &[Vec<Value>],
) -> Result<Vec<FkChildStep>, EngineError> {
    use alloc::collections::{BTreeMap, BTreeSet};
    if to_delete_rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut delete_plan: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    // setnull / setdefault keyed by child_table → (row_idx, col_idx) → optional default
    let mut setnull_plan: BTreeMap<String, BTreeSet<(usize, usize)>> = BTreeMap::new();
    let mut setdefault_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();
    let mut visited: BTreeSet<(String, usize)> = BTreeSet::new();
    for &p in to_delete_positions {
        visited.insert((parent_table_name.to_string(), p));
    }
    let mut work: Vec<(String, Vec<Value>)> = to_delete_rows
        .iter()
        .map(|r| (parent_table_name.to_string(), r.clone()))
        .collect();
    while let Some((cur_parent, parent_row)) = work.pop() {
        for child_name in catalog.table_names() {
            let child = catalog
                .get(&child_name)
                .expect("table_names → catalog.get round-trip is total");
            for fk in &child.schema().foreign_keys {
                if fk.parent_table != cur_parent {
                    continue;
                }
                let parent_key: Vec<&Value> = fk
                    .parent_columns
                    .iter()
                    .map(|&pi| &parent_row[pi])
                    .collect();
                if parent_key.iter().any(|v| matches!(v, Value::Null)) {
                    continue;
                }
                for (child_row_idx, child_row) in child.rows().iter().enumerate() {
                    if child_name == cur_parent
                        && visited.contains(&(child_name.clone(), child_row_idx))
                    {
                        continue;
                    }
                    let matches_key = fk
                        .local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| child_row.values.get(li) == Some(parent_key[i]));
                    if !matches_key {
                        continue;
                    }
                    match fk.on_delete {
                        spg_storage::FkAction::Restrict | spg_storage::FkAction::NoAction => {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "FOREIGN KEY violation: DELETE on {cur_parent:?} is \
                                 restricted by FK from {child_name:?}.{:?}",
                                fk.local_columns,
                            )));
                        }
                        spg_storage::FkAction::Cascade => {
                            if visited.insert((child_name.clone(), child_row_idx)) {
                                delete_plan
                                    .entry(child_name.clone())
                                    .or_default()
                                    .insert(child_row_idx);
                                work.push((child_name.clone(), child_row.values.clone()));
                            }
                        }
                        spg_storage::FkAction::SetNull => {
                            // Verify every local FK column is NULL-able.
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                if !col.nullable {
                                    return Err(EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON DELETE SET NULL: column \
                                         {child_name:?}.{:?} is NOT NULL — cannot SET NULL",
                                        col.name,
                                    )));
                                }
                            }
                            let entry = setnull_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                entry.insert((child_row_idx, li));
                            }
                        }
                        spg_storage::FkAction::SetDefault => {
                            // Resolve the DEFAULT for every local FK col.
                            let entry = setdefault_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                let default = col.default.clone().ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON DELETE SET DEFAULT: column \
                                         {child_name:?}.{:?} has no DEFAULT declared",
                                        col.name,
                                    ))
                                })?;
                                entry.insert((child_row_idx, li), default);
                            }
                        }
                    }
                }
            }
        }
    }
    // Flatten the three plans into the ordered `FkChildStep` list.
    // Deletes are applied last per child (after any null/default
    // re-writes on the same child) so a child row that's both
    // re-written and then cascade-deleted only ends up deleted —
    // but in v7.6.5 SetNull/Cascade never overlap on the same row
    // (a single FK chooses exactly one action), so the order is
    // mostly a precaution.
    let mut steps: Vec<FkChildStep> = Vec::new();
    for (child_table, entries) in setnull_plan {
        let (positions, columns): (Vec<usize>, Vec<usize>) = entries.into_iter().unzip();
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetNull { positions, columns },
        });
    }
    for (child_table, entries) in setdefault_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    for (child_table, positions) in delete_plan {
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::Delete {
                positions: positions.into_iter().collect(),
            },
        });
    }
    Ok(steps)
}

/// v7.6.6 — plan FK fallout for an UPDATE that mutates parent-side
/// PK/UNIQUE columns. Walks every other table whose FK references
/// `parent_table_name`; for each FK whose parent_columns overlap a
/// mutated column, decides the action by `fk.on_update`.
///
///   - RESTRICT / NoAction → error if any child references the OLD
///     value
///   - CASCADE → child FK columns get rewritten to the NEW parent
///     value (a SetNull-style update step with the new value)
///   - SetNull → child FK columns set to NULL
///   - SetDefault → child FK columns set to declared default
///
/// `plan_with_old` is `(row_position, old_values, new_values)` so
/// the planner can detect "did this row's parent key actually
/// change?" — only rows where at least one referenced parent
/// column moved trigger inbound work.
fn plan_fk_parent_updates(
    catalog: &Catalog,
    parent_table_name: &str,
    plan_with_old: &[(usize, Vec<Value>, Vec<Value>)],
) -> Result<Vec<FkChildStep>, EngineError> {
    use alloc::collections::BTreeMap;
    if plan_with_old.is_empty() {
        return Ok(Vec::new());
    }
    // For each child table we may touch, build per-child step
    // lists. UPDATE never deletes children — `delete_plan` stays
    // empty here but is kept structurally aligned with
    // `plan_fk_parent_deletions` for future use.
    let delete_plan: BTreeMap<String, alloc::collections::BTreeSet<usize>> = BTreeMap::new();
    let mut setnull_plan: BTreeMap<String, alloc::collections::BTreeSet<(usize, usize)>> =
        BTreeMap::new();
    let mut setdefault_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();
    // Cascade-update plan: child_table → row_idx → col_idx → new_value
    let mut cascade_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();

    for child_name in catalog.table_names() {
        let child = catalog
            .get(&child_name)
            .expect("table_names → catalog.get total");
        for fk in &child.schema().foreign_keys {
            if fk.parent_table != parent_table_name {
                continue;
            }
            for (_pos, old_row, new_row) in plan_with_old {
                // Did any parent FK column change?
                let key_changed = fk
                    .parent_columns
                    .iter()
                    .any(|&pi| old_row.get(pi) != new_row.get(pi));
                if !key_changed {
                    continue;
                }
                // The OLD parent key — used to find referring children.
                let old_key: Vec<&Value> =
                    fk.parent_columns.iter().map(|&pi| &old_row[pi]).collect();
                if old_key.iter().any(|v| matches!(v, Value::Null)) {
                    // NULL parent has no children — skip.
                    continue;
                }
                let new_key: Vec<&Value> =
                    fk.parent_columns.iter().map(|&pi| &new_row[pi]).collect();
                for (child_row_idx, child_row) in child.rows().iter().enumerate() {
                    // Self-ref same-row updates: a row updating its
                    // own PK doesn't restrict itself.
                    if child_name == parent_table_name
                        && plan_with_old.iter().any(|(p, _, _)| *p == child_row_idx)
                    {
                        continue;
                    }
                    let matches_key = fk
                        .local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| child_row.values.get(li) == Some(old_key[i]));
                    if !matches_key {
                        continue;
                    }
                    match fk.on_update {
                        spg_storage::FkAction::Restrict | spg_storage::FkAction::NoAction => {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "FOREIGN KEY violation: UPDATE on {parent_table_name:?} PK is \
                                 restricted by FK from {child_name:?}.{:?}",
                                fk.local_columns,
                            )));
                        }
                        spg_storage::FkAction::Cascade => {
                            // Rewrite child FK columns to new key.
                            let entry = cascade_plan.entry(child_name.clone()).or_default();
                            for (i, &li) in fk.local_columns.iter().enumerate() {
                                entry.insert((child_row_idx, li), new_key[i].clone());
                            }
                        }
                        spg_storage::FkAction::SetNull => {
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                if !col.nullable {
                                    return Err(EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON UPDATE SET NULL: column \
                                         {child_name:?}.{:?} is NOT NULL",
                                        col.name,
                                    )));
                                }
                            }
                            let entry = setnull_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                entry.insert((child_row_idx, li));
                            }
                        }
                        spg_storage::FkAction::SetDefault => {
                            let entry = setdefault_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                let default = col.default.clone().ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON UPDATE SET DEFAULT: column \
                                         {child_name:?}.{:?} has no DEFAULT",
                                        col.name,
                                    ))
                                })?;
                                entry.insert((child_row_idx, li), default);
                            }
                        }
                    }
                }
            }
        }
    }
    // Flatten into FkChildStep list. UPDATE doesn't produce
    // DeleteSteps (CASCADE on UPDATE just rewrites FK values).
    let mut steps: Vec<FkChildStep> = Vec::new();
    for (child_table, entries) in cascade_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        // We reuse `FkChildAction::SetDefault` for cascade-update:
        // both shapes are "write a known value into specific cells"
        // — `apply_per_cell_writes` doesn't care whether the value
        // came from a DEFAULT declaration or a new parent key.
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    for (child_table, entries) in setnull_plan {
        let (positions, columns): (Vec<usize>, Vec<usize>) = entries.into_iter().unzip();
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetNull { positions, columns },
        });
    }
    for (child_table, entries) in setdefault_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    let _ = delete_plan; // UPDATE never deletes children.
    Ok(steps)
}

/// v7.6.5 — apply one FK child step to the catalog. Encapsulates
/// the three action variants so the DELETE executor stays a
/// simple loop over the planned steps.
fn apply_fk_child_step(catalog: &mut Catalog, step: &FkChildStep) -> Result<(), EngineError> {
    let child = catalog.get_mut(&step.child_table).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: step.child_table.clone(),
        })
    })?;
    match &step.action {
        FkChildAction::Delete { positions } => {
            let _ = child.delete_rows(positions);
        }
        FkChildAction::SetNull { positions, columns } => {
            apply_per_cell_writes(child, positions, columns, |_| Value::Null)?;
        }
        FkChildAction::SetDefault {
            positions,
            columns,
            defaults,
        } => {
            apply_per_cell_writes(child, positions, columns, |i| defaults[i].clone())?;
        }
    }
    Ok(())
}

/// v7.6.5 — write new values into selected child cells via
/// `Table::update_row` (the catalog's existing UPDATE entry).
/// Groups writes by row position so multi-column updates on the
/// same row only call `update_row` once. `value_for(i)` produces
/// the new value for the i-th (position, column) entry.
fn apply_per_cell_writes(
    child: &mut spg_storage::Table,
    positions: &[usize],
    columns: &[usize],
    mut value_for: impl FnMut(usize) -> Value,
) -> Result<(), EngineError> {
    use alloc::collections::BTreeMap;
    let mut by_row: BTreeMap<usize, Vec<(usize, Value)>> = BTreeMap::new();
    for i in 0..positions.len() {
        by_row
            .entry(positions[i])
            .or_default()
            .push((columns[i], value_for(i)));
    }
    for (pos, mutations) in by_row {
        let mut new_values = child.rows()[pos].values.clone();
        for (col, v) in mutations {
            if let Some(slot) = new_values.get_mut(col) {
                *slot = v;
            }
        }
        child
            .update_row(pos, new_values)
            .map_err(EngineError::Storage)?;
    }
    Ok(())
}

fn fk_action_sql_to_storage(a: spg_sql::ast::FkAction) -> spg_storage::FkAction {
    match a {
        spg_sql::ast::FkAction::Restrict => spg_storage::FkAction::Restrict,
        spg_sql::ast::FkAction::Cascade => spg_storage::FkAction::Cascade,
        spg_sql::ast::FkAction::SetNull => spg_storage::FkAction::SetNull,
        spg_sql::ast::FkAction::SetDefault => spg_storage::FkAction::SetDefault,
        spg_sql::ast::FkAction::NoAction => spg_storage::FkAction::NoAction,
    }
}

/// v7.9.21 — resolve a column's DEFAULT for INSERT-time
/// default-fill. Free fn (rather than `&self`) so callers
/// with an active `&mut Table` borrow can still use it.
/// Literal defaults take the cached path (`col.default`);
/// runtime defaults hit `clock_fn` at each call. mailrs G4.
fn resolve_column_default_free(
    col: &ColumnSchema,
    clock_fn: Option<ClockFn>,
) -> Result<Value, EngineError> {
    if let Some(rt) = &col.runtime_default {
        return eval_runtime_default_free(rt, col.ty, clock_fn);
    }
    Ok(col.default.clone().unwrap_or(Value::Null))
}

fn eval_runtime_default_free(
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
fn canonicalize_set_value(
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

fn enforce_enum_label(
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

/// v7.10.4 — decode a BYTEA literal. Accepts:
///   * `\xDEADBEEF` (case-insensitive hex; whitespace stripped)
///   * `Hello\000world` (backslash escape form; `\\` for literal backslash)
///   * Anything else → raw UTF-8 bytes of the input (PG accepts this too).
fn decode_bytea_literal(s: &str) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        // Hex form. Each pair of hex digits → one byte.
        let cleaned: alloc::string::String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        if cleaned.len() % 2 != 0 {
            return Err("odd-length hex literal");
        }
        let mut out = alloc::vec::Vec::with_capacity(cleaned.len() / 2);
        let cleaned_bytes = cleaned.as_bytes();
        for i in (0..cleaned_bytes.len()).step_by(2) {
            let hi = hex_nibble(cleaned_bytes[i])?;
            let lo = hex_nibble(cleaned_bytes[i + 1])?;
            out.push((hi << 4) | lo);
        }
        return Ok(out);
    }
    // Escape form or raw. Walk char-by-char; `\\` and `\NNN` octal
    // sequences decode; anything else is a literal byte.
    let bytes = s.as_bytes();
    let mut out = alloc::vec::Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            if n == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if n.is_ascii_digit()
                && i + 3 < bytes.len()
                && bytes[i + 2].is_ascii_digit()
                && bytes[i + 3].is_ascii_digit()
            {
                let oct = |x: u8| (x - b'0') as u32;
                let v = oct(n) * 64 + oct(bytes[i + 2]) * 8 + oct(bytes[i + 3]);
                if v <= 0xFF {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b);
        i += 1;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, &'static str> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex digit"),
    }
}

/// v7.10.11 — decode a PG TEXT[] external array form
/// (`{a,b,NULL}` with optional double-quoted elements). The
/// engine takes a leading/trailing `{`/`}` and splits at commas.
/// Quoted elements (`"hello, world"`) preserve embedded commas;
/// `\\` and `\"` decode to literal backslash / quote. Plain
/// unquoted `NULL` (case-insensitive) maps to `None`.
/// v7.11.13 — pick the array type for `ARRAY[lit, …]` from the
/// element values. Single-element-type rules:
///   - all NULL / all Text → TextArray
///   - all Int (or Int+NULL) → IntArray
///   - any BigInt without Text → BigIntArray (widening)
///   - any Text → TextArray (fallback; non-string elements
///     render as text)
fn array_literal_widen(items: alloc::vec::Vec<Value>) -> Value {
    let mut has_text = false;
    let mut has_bigint = false;
    let mut has_int = false;
    for v in &items {
        match v {
            Value::Null => {}
            Value::Text(_) | Value::Json(_) => has_text = true,
            Value::BigInt(_) => has_bigint = true,
            Value::Int(_) | Value::SmallInt(_) => has_int = true,
            _ => has_text = true,
        }
    }
    if has_text || (!has_bigint && !has_int) {
        let out: alloc::vec::Vec<Option<alloc::string::String>> = items
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Text(s) | Value::Json(s) => Some(s),
                other => Some(alloc::format!("{other:?}")),
            })
            .collect();
        return Value::TextArray(out);
    }
    if has_bigint {
        let out: alloc::vec::Vec<Option<i64>> = items
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Int(n) => Some(i64::from(n)),
                Value::SmallInt(n) => Some(i64::from(n)),
                Value::BigInt(n) => Some(n),
                _ => unreachable!("widen: unexpected non-integer in BigInt path"),
            })
            .collect();
        return Value::BigIntArray(out);
    }
    let out: alloc::vec::Vec<Option<i32>> = items
        .into_iter()
        .map(|v| match v {
            Value::Null => None,
            Value::Int(n) => Some(n),
            Value::SmallInt(n) => Some(i32::from(n)),
            _ => unreachable!("widen: unexpected non-i32-compatible in Int path"),
        })
        .collect();
    Value::IntArray(out)
}

fn decode_text_array_literal(
    s: &str,
) -> Result<alloc::vec::Vec<Option<alloc::string::String>>, &'static str> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or("TEXT[] literal must be enclosed in '{...}'")?;
    let mut out: alloc::vec::Vec<Option<alloc::string::String>> = alloc::vec::Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        // Skip leading whitespace.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // Quoted element.
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1; // open quote
            let mut buf = alloc::string::String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    buf.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    buf.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return Err("unterminated quoted element");
            }
            i += 1; // close quote
            out.push(Some(buf));
        } else {
            // Unquoted element — read until next comma or end.
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            let raw = inner[start..i].trim();
            if raw.eq_ignore_ascii_case("NULL") {
                out.push(None);
            } else {
                out.push(Some(alloc::string::ToString::to_string(raw)));
            }
        }
        // Skip whitespace, expect comma or end.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b',' {
            return Err("expected ',' between TEXT[] elements");
        }
        i += 1;
    }
    Ok(out)
}

/// v7.10.11 — encode a TEXT[] back into the PG external array
/// form. NULL elements become the literal `NULL`; elements
/// containing commas, quotes, backslashes, or braces are
/// double-quoted with `\\` / `\"` escapes.
fn encode_text_array(items: &[Option<alloc::string::String>]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => {
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars()
                        .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | ' ' | '\t'));
                if needs_quote {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// v7.10.4 — encode BYTEA bytes in PG hex output format
/// (`\x` prefix, lowercase hex pairs). Used by Text-side
/// round-trip + the wire layer's text-mode encoder.
fn encode_bytea_hex(b: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(2 + 2 * b.len());
    out.push_str("\\x");
    for byte in b {
        let hi = byte >> 4;
        let lo = byte & 0x0F;
        out.push(hex_digit(hi));
        out.push(hex_digit(lo));
    }
    out
}

const fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

/// v7.17.0 Phase 3.P0-39 — parse a PG `hstore` text literal into
/// a flat key→value map. Empty string → empty map. Duplicate
/// keys take last-write-wins (matches PG `hstore_in`).
///
/// Accepted shapes (minimal subset):
///   * `'a=>1, b=>2'`            — bareword keys/values
///   * `'"a"=>"1", "b"=>"2"'`    — quoted keys/values
///   * `'a=>NULL'`               — case-insensitive NULL token
///     surfaces as `None` (no quotes around NULL)
///
/// Returns None on parse failure → caller surfaces as hard error.
fn parse_hstore_str(
    s: &str,
) -> Option<Vec<(alloc::string::String, Option<alloc::string::String>)>> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut out: Vec<(alloc::string::String, Option<alloc::string::String>)> = Vec::new();
    let skip_ws = |bytes: &[u8], i: &mut usize| {
        while *i < bytes.len() && matches!(bytes[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    };
    let parse_token = |bytes: &[u8], i: &mut usize| -> Option<alloc::string::String> {
        if *i >= bytes.len() {
            return None;
        }
        if bytes[*i] == b'"' {
            *i += 1;
            let mut out = alloc::string::String::new();
            while *i < bytes.len() {
                match bytes[*i] {
                    b'"' => {
                        *i += 1;
                        return Some(out);
                    }
                    b'\\' if *i + 1 < bytes.len() => {
                        out.push(bytes[*i + 1] as char);
                        *i += 2;
                    }
                    c => {
                        out.push(c as char);
                        *i += 1;
                    }
                }
            }
            None
        } else {
            let start = *i;
            while *i < bytes.len()
                && !matches!(bytes[*i], b' ' | b'\t' | b'\n' | b'\r' | b',' | b'=')
            {
                *i += 1;
            }
            if *i == start {
                return None;
            }
            Some(alloc::str::from_utf8(&bytes[start..*i]).ok()?.to_string())
        }
    };
    skip_ws(bytes, &mut i);
    while i < bytes.len() {
        let key = parse_token(bytes, &mut i)?;
        skip_ws(bytes, &mut i);
        if i + 1 >= bytes.len() || bytes[i] != b'=' || bytes[i + 1] != b'>' {
            return None;
        }
        i += 2;
        skip_ws(bytes, &mut i);
        // Check for unquoted NULL token (case-insensitive).
        let val_token = if i + 4 <= bytes.len()
            && bytes[i..i + 4].eq_ignore_ascii_case(b"NULL")
            && (i + 4 == bytes.len() || matches!(bytes[i + 4], b' ' | b'\t' | b',' | b'\n' | b'\r'))
        {
            i += 4;
            None
        } else {
            Some(parse_token(bytes, &mut i)?)
        };
        // Replace any existing entry with the same key (last-wins).
        if let Some(pos) = out.iter().position(|(k, _)| k == &key) {
            out[pos] = (key, val_token);
        } else {
            out.push((key, val_token));
        }
        skip_ws(bytes, &mut i);
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b',' {
            i += 1;
            skip_ws(bytes, &mut i);
            continue;
        }
        return None;
    }
    Some(out)
}

/// v7.17.0 Phase 3.P0-39 — render a hstore as canonical PG text
/// form `"k"=>"v"` (keys and non-NULL values always quoted;
/// NULL token is bare).
fn format_hstore_str(
    pairs: &[(alloc::string::String, Option<alloc::string::String>)],
) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(k);
        out.push_str("\"=>");
        match v {
            None => out.push_str("NULL"),
            Some(val) => {
                out.push('"');
                out.push_str(val);
                out.push('"');
            }
        }
    }
    out
}

/// v7.17.0 Phase 3.P0-39 — pub re-export so pgwire + sqllogictest
/// share the single hstore renderer.
pub fn format_hstore_text(
    pairs: &[(alloc::string::String, Option<alloc::string::String>)],
) -> alloc::string::String {
    format_hstore_str(pairs)
}

// ─── v7.17.0 Phase 3.P0-40 — 2D array parse + display ─────────

/// Split a PG external 2D-array literal `'{{a,b},{c,d}}'` into
/// per-row token lists. Returns Err on shape mismatch.
fn split_2d_literal(s: &str) -> Result<Vec<Vec<alloc::string::String>>, &'static str> {
    let s = s.trim();
    let outer = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or("missing outer '{...}' braces")?;
    let trimmed = outer.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows: Vec<Vec<alloc::string::String>> = Vec::new();
    let mut i = 0;
    let bytes = trimmed.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'{' {
            return Err("expected '{' opening a row");
        }
        i += 1;
        let row_start = i;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                i += 1;
            }
        }
        if depth != 0 {
            return Err("unbalanced '{...}' in row");
        }
        let row_text = &trimmed[row_start..i];
        i += 1;
        let cells: Vec<alloc::string::String> = if row_text.trim().is_empty() {
            Vec::new()
        } else {
            row_text.split(',').map(|t| t.trim().to_string()).collect()
        };
        rows.push(cells);
    }
    if let Some(first) = rows.first() {
        let cols = first.len();
        for r in &rows {
            if r.len() != cols {
                return Err("ragged 2D array (rows have different column counts)");
            }
        }
    }
    Ok(rows)
}

fn parse_int_2d_literal(s: &str) -> Result<Vec<Vec<Option<i32>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    raw.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        Ok(None)
                    } else {
                        cell.parse::<i32>()
                            .map(Some)
                            .map_err(|_| "invalid int element")
                    }
                })
                .collect()
        })
        .collect()
}

fn parse_bigint_2d_literal(s: &str) -> Result<Vec<Vec<Option<i64>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    raw.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        Ok(None)
                    } else {
                        cell.parse::<i64>()
                            .map(Some)
                            .map_err(|_| "invalid bigint element")
                    }
                })
                .collect()
        })
        .collect()
}

fn parse_text_2d_literal(s: &str) -> Result<Vec<Vec<Option<alloc::string::String>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    Ok(raw
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        None
                    } else {
                        Some(cell.trim_matches('"').to_string())
                    }
                })
                .collect()
        })
        .collect())
}

fn format_int_2d_text(rows: &[Vec<Option<i32>>]) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(n) => out.push_str(&alloc::format!("{n}")),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

fn format_bigint_2d_text(rows: &[Vec<Option<i64>>]) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(n) => out.push_str(&alloc::format!("{n}")),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

fn format_text_2d_text(rows: &[Vec<Option<alloc::string::String>>]) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(s) => out.push_str(s),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

/// v7.17.0 Phase 3.P0-40 — pub re-exports so pgwire + sqllogictest
/// share the single 2D-array renderer.
pub fn format_int_2d_text_pub(rows: &[Vec<Option<i32>>]) -> alloc::string::String {
    format_int_2d_text(rows)
}
pub fn format_bigint_2d_text_pub(rows: &[Vec<Option<i64>>]) -> alloc::string::String {
    format_bigint_2d_text(rows)
}
pub fn format_text_2d_text_pub(
    rows: &[Vec<Option<alloc::string::String>>],
) -> alloc::string::String {
    format_text_2d_text(rows)
}

/// v7.17.0 Phase 3.P0-38 — parse a PG range literal of the form
/// `'[lo,up)'` / `'(lo,up]'` / `'[lo,up]'` / `'(lo,up)'` /
/// `'empty'`. Lower / upper may be empty (unbounded). Returns
/// `None` on any parse failure; caller surfaces as hard error.
fn parse_range_str(s: &str, kind: spg_storage::RangeKind) -> Option<Value> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("empty") {
        return Some(Value::Range {
            kind,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
            empty: true,
        });
    }
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let lower_inc = match bytes[0] {
        b'[' => true,
        b'(' => false,
        _ => return None,
    };
    let upper_inc = match bytes[bytes.len() - 1] {
        b']' => true,
        b')' => false,
        _ => return None,
    };
    let inner = &s[1..s.len() - 1];
    let (lo_text, up_text) = inner.split_once(',')?;
    let lower = if lo_text.is_empty() {
        None
    } else {
        Some(alloc::boxed::Box::new(parse_range_element(lo_text, kind)?))
    };
    let upper = if up_text.is_empty() {
        None
    } else {
        Some(alloc::boxed::Box::new(parse_range_element(up_text, kind)?))
    };
    Some(Value::Range {
        kind,
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty: false,
    })
}

/// v7.17.0 Phase 3.P0-38 — parse a single range bound text into
/// the matching element Value for the RangeKind.
fn parse_range_element(text: &str, kind: spg_storage::RangeKind) -> Option<Value> {
    let text = text.trim().trim_matches('"');
    use spg_storage::RangeKind as K;
    match kind {
        K::Int4 => text.parse::<i32>().ok().map(Value::Int),
        K::Int8 => text.parse::<i64>().ok().map(Value::BigInt),
        K::Num => {
            // Reuse the Numeric parse via the engine's text-coercion
            // path; bail to None on failure.
            let dot = text.find('.');
            let scale: u8 = dot.map_or(0, |p| (text.len() - p - 1) as u8);
            let digits: alloc::string::String = text
                .chars()
                .filter(|c| *c == '-' || c.is_ascii_digit())
                .collect();
            let scaled: i128 = digits.parse().ok()?;
            Some(Value::Numeric { scaled, scale })
        }
        K::Ts | K::TsTz => {
            // Reuse the existing timestamp parse path. v7.17.0
            // expects `'YYYY-MM-DD HH:MM:SS[.ffffff]'` in range
            // bounds (TZ offset on TsTz is OOS for the initial
            // P0-38; ship plain Timestamp shape).
            crate::eval::parse_timestamp_literal(text).map(Value::Timestamp)
        }
        K::Date => crate::eval::parse_date_literal(text).map(Value::Date),
    }
}

/// v7.17.0 Phase 3.P0-38 — render a Range value as its canonical
/// PG text form. Re-exported via [`format_range_text`] for use
/// from spg-server's pgwire layer.
pub fn format_range_text(v: &Value) -> alloc::string::String {
    format_range_str(v)
}

fn format_range_str(v: &Value) -> alloc::string::String {
    let Value::Range {
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty,
        ..
    } = v
    else {
        return alloc::string::String::new();
    };
    if *empty {
        return "empty".into();
    }
    let mut out = alloc::string::String::new();
    out.push(if *lower_inc { '[' } else { '(' });
    if let Some(l) = lower {
        out.push_str(&format_range_element(l));
    }
    out.push(',');
    if let Some(u) = upper {
        out.push_str(&format_range_element(u));
    }
    out.push(if *upper_inc { ']' } else { ')' });
    out
}

fn format_range_element(v: &Value) -> alloc::string::String {
    match v {
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        Value::Date(d) => crate::eval::format_date(*d),
        Value::Timestamp(t) => crate::eval::format_timestamp(*t),
        Value::Numeric { scaled, scale } => crate::eval::format_numeric(*scaled, *scale),
        other => alloc::format!("{other:?}"),
    }
}

/// v7.17.0 Phase 3.P0-35 — parse a PG `money` literal into i64
/// cents. Accepts:
///   * Optional leading `-` (negative)
///   * Optional `$` prefix
///   * Integer portion with optional `,` thousands separators
///   * Optional `.` followed by 1-2 digits (cents); 1 digit
///     auto-pads to 2 (`.5` → 50 cents).
///
/// Returns None on any parse failure — caller surfaces as hard
/// SQL error.
fn parse_money_str(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r.trim_start()),
        None => (false, s),
    };
    let rest = rest.strip_prefix('$').unwrap_or(rest).trim_start();
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    if int_part.is_empty() {
        return None;
    }
    // Validate + strip commas from the integer portion.
    let mut int_digits = alloc::string::String::with_capacity(int_part.len());
    for b in int_part.bytes() {
        match b {
            b',' => {}
            b'0'..=b'9' => int_digits.push(b as char),
            _ => return None,
        }
    }
    if int_digits.is_empty() {
        return None;
    }
    let dollars: i64 = int_digits.parse().ok()?;
    let cents: i64 = match frac_part {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let padded = if f.len() == 1 {
                alloc::format!("{f}0")
            } else {
                f.to_string()
            };
            padded.parse().ok()?
        }
    };
    let total = dollars.checked_mul(100)?.checked_add(cents)?;
    Some(if neg { -total } else { total })
}

/// v7.17.0 Phase 3.P0-34 — parse a PG `timetz` literal
/// `HH:MM:SS[.fraction]±HH[:MM]` into (us, offset_secs).
///
/// The offset suffix is MANDATORY: SPG doesn't have a session TZ
/// wired into eval, so a bare `HH:MM:SS` literal would be
/// ambiguous. Returns None for any parse failure or out-of-range
/// component — caller surfaces as a hard SQL error.
///
/// Offset range: ±14 hours (±50400 seconds), matching PG's
/// internal limit.
fn parse_timetz_str(s: &str) -> Option<(i64, i32)> {
    let s = s.trim();
    // Find the offset sign — scan from right since the time part
    // never contains '+' / '-' (after the optional fractional dot
    // it's all digits and ':').
    let bytes = s.as_bytes();
    let sign_pos = bytes
        .iter()
        .enumerate()
        .rev()
        .find(|&(_, &b)| b == b'+' || b == b'-')
        .map(|(i, _)| i)?;
    if sign_pos == 0 {
        return None; // bare sign — no time component
    }
    let time_part = &s[..sign_pos];
    let offset_part = &s[sign_pos..];
    let us = parse_time_str(time_part)?;
    let sign: i32 = if offset_part.starts_with('+') { 1 } else { -1 };
    let offset_body = &offset_part[1..];
    let (hh_str, mm_str) = match offset_body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (offset_body, "0"),
    };
    let hh: i32 = hh_str.parse().ok()?;
    let mm: i32 = mm_str.parse().ok()?;
    if !(0..=14).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    let total = sign * (hh * 3600 + mm * 60);
    if total.abs() > 50_400 {
        return None;
    }
    Some((us, total))
}

/// v7.17.0 Phase 3.P0-33 — funnel an integer literal through MySQL
/// YEAR range validation: 0 sentinel or 1901..=2155. Out-of-range
/// surfaces as a hard SQL error (no silent truncation, mirrors PG
/// `time_in` / `uuid_in` discipline).
fn coerce_int_to_year(n: i64, col_name: &str) -> Result<Value, EngineError> {
    if n == 0 || (1901..=2155).contains(&n) {
        // u16::try_from cannot fail in this range; the cast also
        // covers the 0 sentinel.
        return Ok(Value::Year(n as u16));
    }
    Err(EngineError::Eval(EvalError::TypeMismatch {
        detail: alloc::format!(
            "year value out of range: {n} (column `{col_name}`; \
             MySQL accepts 0 or 1901..=2155)"
        ),
    }))
}

/// v7.17.0 Phase 3.P0-32 — parse a PG `time` literal
/// `HH:MM:SS[.fraction]` into microseconds since 00:00:00.
///
/// Accepts:
///   * `HH:MM:SS`            — exact-second precision
///   * `HH:MM:SS.f` .. `.ffffff` — 1-6 fractional digits, right-padded
///     with zeros to microseconds
///
/// Range: hour 0..=23, minute 0..=59, second 0..=59. Anything else
/// returns None — caller surfaces as a hard SQL error (no silent
/// truncation, matches PG's `time_in` behaviour).
fn parse_time_str(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, frac) = match s.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (s, None),
    };
    let mut parts = hms.split(':');
    let hh: u32 = parts.next()?.parse().ok()?;
    let mm: u32 = parts.next()?.parse().ok()?;
    let ss: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let frac_us: i64 = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Right-pad with zeros so '.5' = 500000 µsec.
            let mut padded = alloc::string::String::with_capacity(6);
            padded.push_str(f);
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
    };
    Some(
        i64::from(hh) * 3_600_000_000
            + i64::from(mm) * 60_000_000
            + i64::from(ss) * 1_000_000
            + frac_us,
    )
}

const fn column_type_to_data_type(t: ColumnTypeName) -> DataType {
    match t {
        ColumnTypeName::SmallInt => DataType::SmallInt,
        ColumnTypeName::Int => DataType::Int,
        ColumnTypeName::BigInt => DataType::BigInt,
        ColumnTypeName::Float => DataType::Float,
        ColumnTypeName::Text => DataType::Text,
        ColumnTypeName::Varchar(n) => DataType::Varchar(n),
        ColumnTypeName::Char(n) => DataType::Char(n),
        ColumnTypeName::Bool => DataType::Bool,
        ColumnTypeName::Vector { dim, encoding } => DataType::Vector {
            dim,
            encoding: match encoding {
                SqlVecEncoding::F32 => VecEncoding::F32,
                SqlVecEncoding::Sq8 => VecEncoding::Sq8,
                SqlVecEncoding::F16 => VecEncoding::F16,
            },
        },
        ColumnTypeName::Numeric(precision, scale) => DataType::Numeric { precision, scale },
        ColumnTypeName::Date => DataType::Date,
        ColumnTypeName::Timestamp => DataType::Timestamp,
        ColumnTypeName::Timestamptz => DataType::Timestamptz,
        ColumnTypeName::Json => DataType::Json,
        ColumnTypeName::Jsonb => DataType::Jsonb,
        ColumnTypeName::Bytes => DataType::Bytes,
        ColumnTypeName::TextArray => DataType::TextArray,
        ColumnTypeName::IntArray => DataType::IntArray,
        ColumnTypeName::BigIntArray => DataType::BigIntArray,
        ColumnTypeName::TsVector => DataType::TsVector,
        ColumnTypeName::TsQuery => DataType::TsQuery,
        ColumnTypeName::Uuid => DataType::Uuid,
        ColumnTypeName::Time => DataType::Time,
        ColumnTypeName::Year => DataType::Year,
        ColumnTypeName::TimeTz => DataType::TimeTz,
        ColumnTypeName::Money => DataType::Money,
        ColumnTypeName::Range(k) => DataType::Range(match k {
            spg_sql::ast::RangeKindAst::Int4 => spg_storage::RangeKind::Int4,
            spg_sql::ast::RangeKindAst::Int8 => spg_storage::RangeKind::Int8,
            spg_sql::ast::RangeKindAst::Num => spg_storage::RangeKind::Num,
            spg_sql::ast::RangeKindAst::Ts => spg_storage::RangeKind::Ts,
            spg_sql::ast::RangeKindAst::TsTz => spg_storage::RangeKind::TsTz,
            spg_sql::ast::RangeKindAst::Date => spg_storage::RangeKind::Date,
        }),
        ColumnTypeName::Hstore => DataType::Hstore,
        ColumnTypeName::IntArray2D => DataType::IntArray2D,
        ColumnTypeName::BigIntArray2D => DataType::BigIntArray2D,
        ColumnTypeName::TextArray2D => DataType::TextArray2D,
    }
}

/// Convert an INSERT VALUES expression to a storage Value. Supports literal
/// expressions, unary-minus over numeric literals, and pgvector-style
/// `'[..]'::vector` cast (v1.2). Anything more complex returns `Unsupported`.
fn literal_expr_to_value(expr: Expr) -> Result<Value, EngineError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Cast { expr, target } => {
            let inner_value = literal_expr_to_value(*expr)?;
            crate::eval::cast_value(inner_value, target).map_err(EngineError::Eval)
        }
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => match *expr {
            Expr::Literal(Literal::Integer(n)) => {
                // Fold to i32 if it fits, else BigInt. Parser emits Integer(i64)
                // — overflow on negate of i64::MIN is the one edge case.
                let neg = n.checked_neg().ok_or_else(|| {
                    EngineError::Unsupported("integer literal overflow on negation".into())
                })?;
                Ok(int_value_for(neg))
            }
            Expr::Literal(Literal::Float(x)) => Ok(Value::Float(-x)),
            other => Err(EngineError::Unsupported(alloc::format!(
                "unary minus over non-literal expression: {other:?}"
            ))),
        },
        // v7.10.10 — `ARRAY[lit, lit, …]` constructor accepted at
        // INSERT-time. Each element must reduce to a Value through
        // `literal_expr_to_value`; NULL elements become `None`.
        // v7.11.13 — deduce shape from element values: all Int →
        // IntArray; any BigInt → BigIntArray (widening); any Text
        // → TextArray. Cast targets (`ARRAY[]::INT[]`) flow through
        // the outer Cast arm before reaching here and re-coerce.
        Expr::Array(items) => {
            let mut materialised: alloc::vec::Vec<Value> =
                alloc::vec::Vec::with_capacity(items.len());
            for elem in items {
                materialised.push(literal_expr_to_value(elem)?);
            }
            Ok(array_literal_widen(materialised))
        }
        // Any other Expr shape — fall back to a general evaluation
        // against an empty row + empty schema. This unblocks the
        // app-common patterns where INSERT VALUES carries a
        // non-correlated function call:
        //   INSERT INTO t VALUES (concat('U-', 42))
        //   INSERT INTO t VALUES (now())
        //   INSERT INTO t VALUES (format('%s-%s', 'a', 'b'))
        // Any expression that references a column or `$N`
        // placeholder fails cleanly inside `eval_expr` with a
        // descriptive error; literals + casts + ARRAY[…] continue
        // to take the fast paths above so the hot INSERT path is
        // unchanged on the common case.
        other => {
            let empty_schema: alloc::vec::Vec<spg_storage::ColumnSchema> = alloc::vec::Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let empty_row = spg_storage::Row::new(alloc::vec::Vec::new());
            crate::eval::eval_expr(&other, &empty_row, &ctx).map_err(EngineError::Eval)
        }
    }
}

fn literal_to_value(l: Literal) -> Value {
    match l {
        Literal::Integer(n) => int_value_for(n),
        Literal::Float(x) => Value::Float(x),
        Literal::String(s) => Value::Text(s),
        Literal::Bool(b) => Value::Bool(b),
        Literal::Null => Value::Null,
        Literal::Vector(v) => Value::Vector(v),
        Literal::TextArray(items) => Value::TextArray(items),
        Literal::IntArray(items) => Value::IntArray(items),
        Literal::BigIntArray(items) => Value::BigIntArray(items),
        Literal::Interval { months, micros, .. } => Value::Interval { months, micros },
    }
}

/// Pick `Int` (`i32`) when the literal fits, else `BigInt`. `INT` vs `BIGINT`
/// columns will still enforce the right tag downstream — this is just the
/// default we synthesise from an unannotated integer literal.
fn int_value_for(n: i64) -> Value {
    if let Ok(small) = i32::try_from(n) {
        Value::Int(small)
    } else {
        Value::BigInt(n)
    }
}

/// Widen / narrow `v` to fit `expected`. Numerics permit safe widening
/// (`Int → BigInt`, `Int/BigInt → Float`) and best-effort narrowing
/// (`BigInt → Int` succeeds only when the value fits in `i32`). Everything
/// else returns `TypeMismatch` carrying the column name for caller diagnostics.
/// `NULL` is always permitted; the nullability check happens later in storage.
#[allow(clippy::too_many_lines)]
/// v7.17.0 Phase 4.4 — reject negative integer values on UNSIGNED
/// columns. Called after `coerce_value` at each INSERT / UPDATE
/// site that has ColumnSchema context. NULL passes through (a
/// nullable UNSIGNED column can legitimately hold NULL).
fn check_unsigned_range(
    v: &Value,
    schema: &ColumnSchema,
    position: usize,
) -> Result<(), EngineError> {
    if !schema.is_unsigned {
        return Ok(());
    }
    let n = match v {
        Value::SmallInt(x) => i64::from(*x),
        Value::Int(x) => i64::from(*x),
        Value::BigInt(x) => *x,
        _ => return Ok(()), // non-integer cells (NULL, default) skip
    };
    if n < 0 {
        return Err(EngineError::Unsupported(alloc::format!(
            "column {:?} is UNSIGNED but got negative value {n} at position {position}",
            schema.name
        )));
    }
    Ok(())
}

fn coerce_value(
    v: Value,
    expected: DataType,
    col_name: &str,
    position: usize,
) -> Result<Value, EngineError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let actual = v.data_type().expect("non-null");
    if actual == expected {
        return Ok(v);
    }
    let coerced = match (v, expected) {
        (Value::Int(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
        (Value::Int(n), DataType::Float) => Some(Value::Float(f64::from(n))),
        (Value::Int(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
        (Value::Int(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::SmallInt(n), DataType::Int) => Some(Value::Int(i32::from(n))),
        (Value::SmallInt(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
        (Value::SmallInt(n), DataType::Float) => Some(Value::Float(f64::from(n))),
        (Value::SmallInt(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::BigInt(n), DataType::Int) => i32::try_from(n).ok().map(Value::Int),
        (Value::BigInt(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
        #[allow(clippy::cast_precision_loss)]
        (Value::BigInt(n), DataType::Float) => Some(Value::Float(n as f64)),
        (Value::BigInt(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::Float(x), DataType::Numeric { precision, scale }) => {
            Some(numeric_from_float(x, precision, scale, col_name)?)
        }
        // v7.17.0 Phase 3.P0-67 — Text → NUMERIC. Parse a
        // canonical decimal text (`"-1234.56"` / `"42"` /
        // `"0.0001"`) into `(mantissa, source_scale)` and rescale
        // to the column's declared scale. Required for prepared
        // binds: `value_to_literal` flattens a Value::Numeric
        // into a TEXT literal because Literal carries no native
        // Numeric variant, so the placeholder substitution path
        // reaches coerce_value as Text → Numeric. Without this
        // arm the round-trip surfaces a TypeMismatch even though
        // the cell already left the engine as a valid Numeric.
        (Value::Text(s), DataType::Numeric { precision, scale }) => {
            let Some((mantissa, src_scale)) = parse_numeric_text(&s) else {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as NUMERIC for column `{col_name}`"),
                }));
            };
            Some(numeric_rescale(
                mantissa, src_scale, precision, scale, col_name,
            )?)
        }
        // Text → DATE / TIMESTAMP: parse canonical text forms.
        (Value::Text(s), DataType::Date) => {
            let d = eval::parse_date_literal(&s).ok_or_else(|| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as DATE for column `{col_name}`"),
                })
            })?;
            Some(Value::Date(d))
        }
        // v7.14.0 — MySQL DEFAULT clauses quote integer / float
        // / boolean literals (`DEFAULT '0'`, `DEFAULT '1'`,
        // `DEFAULT '3.14'`, `DEFAULT 'true'`). Coerce the text
        // form to the column's numeric / bool type at DEFAULT-
        // installation time so the storage check sees a typed
        // value. Parse failures fall through to TypeMismatch.
        (Value::Text(s), DataType::SmallInt) => s.parse::<i16>().ok().map(Value::SmallInt),
        (Value::Text(s), DataType::Int) => s.parse::<i32>().ok().map(Value::Int),
        (Value::Text(s), DataType::BigInt) => s.parse::<i64>().ok().map(Value::BigInt),
        (Value::Text(s), DataType::Float) => s.parse::<f64>().ok().map(Value::Float),
        (Value::Text(s), DataType::Bool) => match s.to_ascii_lowercase().as_str() {
            "0" | "false" | "f" | "no" | "off" => Some(Value::Bool(false)),
            "1" | "true" | "t" | "yes" | "on" => Some(Value::Bool(true)),
            _ => None,
        },
        // v7.17.0 Phase 3.P0-46 — MySQL TINYINT(1) (which Phase 4.3
        // classifies as DataType::Bool) is the storage shape every
        // mysqldump-restored boolean column lands in. mysqldump emits
        // the values as integer `0` / `1` literals, so int → bool
        // coerce on INSERT is required for a 0-change cutover. MySQL's
        // rule is "any non-zero is truthy"; we follow that for all
        // signed int widths so the same coerce path serves an
        // explicit `BOOLEAN` column too.
        (Value::Int(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        (Value::SmallInt(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        (Value::BigInt(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        // v4.9: Text ↔ JSON coercion. No structural validation —
        // any text literal is accepted; the responsibility for
        // valid JSON lies with the producer.
        (Value::Text(s), DataType::Json | DataType::Jsonb) => Some(Value::Json(s)),
        (Value::Json(s), DataType::Text) => Some(Value::Text(s)),
        // v7.13.3 — mailrs round-7 S10. SPG's storage represents
        // both JSON and JSONB on-disk as `Value::Json(String)` —
        // they share the underlying text payload. The cast
        // `'<text>'::jsonb` produces a Value::Json that needs to
        // satisfy a DataType::Jsonb column. Identity coerce in
        // both directions so JSON ↔ JSONB assignments work at all
        // INSERT / ALTER COLUMN TYPE / DEFAULT contexts.
        (Value::Json(s), DataType::Jsonb | DataType::Json) => Some(Value::Json(s)),
        // v7.10.4 — Text → BYTEA. Decode PG-style literal forms:
        //   - Hex:    `\x48656c6c6f`  (case-insensitive hex pairs)
        //   - Escape: `Hello\\000world`  (backslash + octal triples)
        //   - Plain:  any string → raw UTF-8 bytes (PG also accepts)
        // Errors surface as TypeMismatch so the operator gets a
        // clear "this literal isn't a bytea literal" hint.
        (Value::Text(s), DataType::Bytes) => {
            let bytes = decode_bytea_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as BYTEA for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::Bytes(bytes))
        }
        // v7.10.4 — BYTEA → Text round-trip uses the PG hex
        // output (lowercase, `\x` prefix). Important when a
        // SELECT pulls a bytea cell through a Text column path.
        (Value::Bytes(b), DataType::Text) => Some(Value::Text(encode_bytea_hex(&b))),
        // v7.17.0 — Text → UUID. PG accepts canonical hyphenated,
        // unhyphenated, uppercase, and `{...}`-braced forms; we
        // funnel all four through `spg_storage::parse_uuid_str`.
        // A malformed literal surfaces as a SQL TypeMismatch
        // rather than silently inserting garbage — `0-change
        // cutover` requires that an app inserting bad UUID text
        // sees the same hard error PG would raise.
        (Value::Text(s), DataType::Uuid) => match spg_storage::parse_uuid_str(&s) {
            Some(b) => Some(Value::Uuid(b)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type uuid: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.17.0 — UUID → Text canonical 8-4-4-4-12 lowercase.
        // Surfaces when a SELECT plucks a uuid cell through a
        // Text column path (e.g. INSERT INTO log SELECT id::text
        // FROM other_table).
        (Value::Uuid(b), DataType::Text) => Some(Value::Text(spg_storage::format_uuid(&b))),
        // v7.17.0 Phase 3.P0-32 — Text → TIME. Accepts
        // `HH:MM:SS` and `HH:MM:SS.ffffff` (1-6 fractional digits).
        // Out-of-range hour/min/sec is a hard SQL error (no
        // silent truncation — same 0-change-cutover discipline
        // we apply to UUID).
        (Value::Text(s), DataType::Time) => match parse_time_str(&s) {
            Some(us) => Some(Value::Time(us)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type time: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.17.0 Phase 3.P0-32 — TIME → Text canonical `HH:MM:SS[.ffffff]`.
        (Value::Time(us), DataType::Text) => Some(Value::Text(eval::format_time(us))),
        // v7.17.0 Phase 3.P0-33 — int / bigint → YEAR. Range
        // check enforces the MySQL canonical 1901..=2155 + 0
        // sentinel; out-of-range is a hard SQL error (no silent
        // truncation, mirrors P0-32 / P0-25 discipline).
        (Value::SmallInt(n), DataType::Year) => Some(coerce_int_to_year(i64::from(n), col_name)?),
        (Value::Int(n), DataType::Year) => Some(coerce_int_to_year(i64::from(n), col_name)?),
        (Value::BigInt(n), DataType::Year) => Some(coerce_int_to_year(n, col_name)?),
        // Text → YEAR. Accepts the 4-digit decimal form only;
        // two-digit YEAR (`'99'` → 1999) was deprecated in MySQL
        // 5.7 and is out of scope for v7.17.0.
        (Value::Text(s), DataType::Year) => match s.trim().parse::<i64>() {
            Ok(n) => Some(coerce_int_to_year(n, col_name)?),
            Err(_) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type year: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // YEAR → Text 4-digit zero-padded.
        (Value::Year(y), DataType::Text) => Some(Value::Text(alloc::format!("{y:04}"))),
        // v7.17.0 Phase 3.P0-34 — Text → TIMETZ. Mandatory
        // signed offset suffix; missing offset is a hard error
        // (SPG has no session TZ wired into eval, unlike PG).
        (Value::Text(s), DataType::TimeTz) => match parse_timetz_str(&s) {
            Some((us, offset_secs)) => Some(Value::TimeTz { us, offset_secs }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type time with time zone: \
                         {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // TIMETZ → Text canonical `HH:MM:SS[.ffffff]±HH[:MM]`.
        (Value::TimeTz { us, offset_secs }, DataType::Text) => {
            Some(Value::Text(eval::format_timetz(us, offset_secs)))
        }
        // v7.17.0 Phase 3.P0-35 — Text → MONEY. Accepts `$N.NN`,
        // `$N,NNN.NN`, optional leading `-`. Bare numeric literals
        // arrive via the Int/BigInt/Float/Numeric arms below.
        (Value::Text(s), DataType::Money) => match parse_money_str(&s) {
            Some(c) => Some(Value::Money(c)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type money: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Int / BigInt / SmallInt / Float / Numeric → MONEY.
        // Bare numeric literal is interpreted as a major-unit
        // amount (matches PG: `100`::money → $100.00 = 10000 cents).
        (Value::SmallInt(n), DataType::Money) => {
            Some(Value::Money(i64::from(n).saturating_mul(100)))
        }
        (Value::Int(n), DataType::Money) => Some(Value::Money(i64::from(n).saturating_mul(100))),
        (Value::BigInt(n), DataType::Money) => Some(Value::Money(n.saturating_mul(100))),
        (Value::Float(x), DataType::Money) => {
            // Round half-away-from-zero to cents (no_std — no
            // `f64::round`, so hand-roll via biased truncation).
            let scaled = x * 100.0;
            let cents = if scaled >= 0.0 {
                (scaled + 0.5) as i64
            } else {
                (scaled - 0.5) as i64
            };
            Some(Value::Money(cents))
        }
        (Value::Numeric { scaled, scale }, DataType::Money) => {
            // Convert exact decimal to cents (scale 2). If scale > 2,
            // round half-away-from-zero. If scale < 2, multiply up.
            let cents = if scale == 2 {
                scaled
            } else if scale < 2 {
                let mult = 10_i128.pow(u32::from(2 - scale));
                scaled.saturating_mul(mult)
            } else {
                let div = 10_i128.pow(u32::from(scale - 2));
                let half = div / 2;
                let bias = if scaled >= 0 { half } else { -half };
                (scaled + bias) / div
            };
            Some(Value::Money(i64::try_from(cents).unwrap_or(i64::MAX)))
        }
        // MONEY → Text canonical `$N,NNN.CC`.
        (Value::Money(c), DataType::Text) => Some(Value::Text(eval::format_money(c))),
        // v7.17.0 Phase 3.P0-38 — Text → Range. Accepts canonical
        // PG forms: `'empty'`, `'[a,b)'`, `'(a,b]'`, `'[a,b]'`,
        // `'(a,b)'`, with empty lower or upper for unbounded.
        (Value::Text(s), DataType::Range(kind)) => match parse_range_str(&s, kind) {
            Some(v) => Some(v),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for range type: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Range → Text canonical form (`[a,b)`, `'empty'`, etc).
        (v @ Value::Range { .. }, DataType::Text) => Some(Value::Text(format_range_str(&v))),
        // v7.17.0 Phase 3.P0-39 — Text → Hstore.
        (Value::Text(s), DataType::Hstore) => match parse_hstore_str(&s) {
            Some(pairs) => Some(Value::Hstore(pairs)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type hstore: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Hstore → Text canonical `"k"=>"v"` form.
        (Value::Hstore(pairs), DataType::Text) => Some(Value::Text(format_hstore_str(&pairs))),
        // v7.17.0 Phase 3.P0-40 — Text → 2D arrays via PG
        // external `'{{a,b},{c,d}}'` literal.
        (Value::Text(s), DataType::IntArray2D) => match parse_int_2d_literal(&s) {
            Ok(m) => Some(Value::IntArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for INT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::BigIntArray2D) => match parse_bigint_2d_literal(&s) {
            Ok(m) => Some(Value::BigIntArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for BIGINT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::TextArray2D) => match parse_text_2d_literal(&s) {
            Ok(m) => Some(Value::TextArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for TEXT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        // 2D arrays → Text canonical nested form.
        (Value::IntArray2D(rows), DataType::Text) => Some(Value::Text(format_int_2d_text(&rows))),
        (Value::BigIntArray2D(rows), DataType::Text) => {
            Some(Value::Text(format_bigint_2d_text(&rows)))
        }
        (Value::TextArray2D(rows), DataType::Text) => Some(Value::Text(format_text_2d_text(&rows))),
        // v7.10.11 — Text → TEXT[]. Decode PG's external array
        // form `'{a,b,NULL}'`. NULL element token (case-insensitive)
        // is the literal `NULL`; everything else is a quoted or
        // unquoted text element. mailrs `'{label1,label2}'::TEXT[]`.
        (Value::Text(s), DataType::TextArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TEXT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::TextArray(arr))
        }
        // v7.16.0 — Text → IntArray / BigIntArray for the
        // spg-sqlx Bind path. Decode the PG external form
        // `{1,2,3}` as a TEXT array first, then parse each
        // element as int. Same shape as the TextArray decode
        // above with an element-wise narrow.
        (Value::Text(s), DataType::IntArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as INT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            let mut out: Vec<Option<i32>> = Vec::with_capacity(arr.len());
            for elem in arr {
                match elem {
                    None => out.push(None),
                    Some(t) => {
                        let n: i32 = t.parse().map_err(|_| {
                            EngineError::Eval(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "cannot parse {t:?} as INT element for `{col_name}`"
                                ),
                            })
                        })?;
                        out.push(Some(n));
                    }
                }
            }
            Some(Value::IntArray(out))
        }
        (Value::Text(s), DataType::BigIntArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as BIGINT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            let mut out: Vec<Option<i64>> = Vec::with_capacity(arr.len());
            for elem in arr {
                match elem {
                    None => out.push(None),
                    Some(t) => {
                        let n: i64 = t.parse().map_err(|_| {
                            EngineError::Eval(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "cannot parse {t:?} as BIGINT element for `{col_name}`"
                                ),
                            })
                        })?;
                        out.push(Some(n));
                    }
                }
            }
            Some(Value::BigIntArray(out))
        }
        // v7.10.11 — TEXT[] → Text round-trip uses PG's
        // external array form (`{a,b,NULL}`). Lets a SELECT
        // pull an array column through any Text-side codepath.
        (Value::TextArray(items), DataType::Text) => Some(Value::Text(encode_text_array(&items))),
        // v7.17.0 Phase 3.P0-68 — Text → VECTOR auto-coerce.
        // Matches the existing Text → TsVector arm and the
        // `::vector` cast: PG-canonical pgvector external form
        // (`'[1, 2, -3]'`) becomes a typed Vector value at the
        // column boundary. Dim mismatch surfaces as TypeMismatch.
        // For SQ8 / HALF encodings we chain through the standard
        // quantise helpers so the storage shape matches the
        // declared encoding without a second coerce pass.
        (Value::Text(s), DataType::Vector { dim, encoding }) => {
            let parsed = eval::parse_vector_text(&s).ok_or_else(|| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as VECTOR for column `{col_name}`"),
                })
            })?;
            if parsed.len() != dim as usize {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "VECTOR({dim}) column `{col_name}` rejects literal of length {}",
                        parsed.len()
                    ),
                }));
            }
            Some(match encoding {
                VecEncoding::F32 => Value::Vector(parsed),
                VecEncoding::Sq8 => Value::Sq8Vector(spg_storage::quantize::quantize(&parsed)),
                VecEncoding::F16 => {
                    Value::HalfVector(spg_storage::halfvec::HalfVector::from_f32_slice(&parsed))
                }
            })
        }
        // v7.16.1 — Text → TSVECTOR auto-coerce for the
        // INSERT-side wire path (mailrs round-9 A.2.a). PG
        // implicitly promotes the TEXT literal at INSERT into a
        // TSVECTOR column; SPG previously rejected with a hard
        // type mismatch, blocking 23,276 pg_dump rows into
        // `messages.search_vector`. We route through the same
        // `decode_tsvector_external` the `::tsvector` cast
        // already uses, so PG-canonical forms (`'word'`,
        // `'word:1A,2B'`, multi-lexeme, empty `''`) all parse.
        (Value::Text(s), DataType::TsVector) => {
            let lexs = eval::decode_tsvector_external(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TSVECTOR for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::TsVector(lexs))
        }
        (Value::Text(s), DataType::Timestamp | DataType::Timestamptz) => {
            let t = eval::parse_timestamp_literal(&s).ok_or_else(|| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TIMESTAMP for column `{col_name}`"
                    ),
                })
            })?;
            Some(Value::Timestamp(t))
        }
        // DATE ↔ TIMESTAMP convertibility (DATE → midnight,
        // TIMESTAMP → day truncation).
        (Value::Date(d), DataType::Timestamp | DataType::Timestamptz) => {
            Some(Value::Timestamp(i64::from(d) * 86_400_000_000))
        }
        // v7.9.21 — Value::Timestamp lands in either Timestamp
        // or Timestamptz columns; the on-disk layout is the
        // same i64 microseconds UTC.
        (Value::Timestamp(t), DataType::Timestamptz) => Some(Value::Timestamp(t)),
        (Value::Timestamp(t), DataType::Date) => {
            let days = t.div_euclid(86_400_000_000);
            i32::try_from(days).ok().map(Value::Date)
        }
        (
            Value::Numeric {
                scaled,
                scale: src_scale,
            },
            DataType::Numeric { precision, scale },
        ) => Some(numeric_rescale(
            scaled, src_scale, precision, scale, col_name,
        )?),
        #[allow(clippy::cast_precision_loss)]
        (Value::Numeric { scaled, scale }, DataType::Float) => {
            let mut div = 1.0_f64;
            for _ in 0..scale {
                div *= 10.0;
            }
            Some(Value::Float((scaled as f64) / div))
        }
        (Value::Numeric { scaled, scale }, DataType::Int) => {
            let truncated = numeric_truncate_to_integer(scaled, scale);
            i32::try_from(truncated).ok().map(Value::Int)
        }
        (Value::Numeric { scaled, scale }, DataType::BigInt) => {
            let truncated = numeric_truncate_to_integer(scaled, scale);
            i64::try_from(truncated).ok().map(Value::BigInt)
        }
        (Value::Numeric { scaled, scale }, DataType::SmallInt) => {
            let truncated = numeric_truncate_to_integer(scaled, scale);
            i16::try_from(truncated).ok().map(Value::SmallInt)
        }
        // VARCHAR(n) enforces an upper bound on character count.
        (Value::Text(s), DataType::Varchar(max)) => {
            if u32::try_from(s.chars().count()).unwrap_or(u32::MAX) <= max {
                Some(Value::Text(s))
            } else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "value for VARCHAR({max}) column `{col_name}` exceeds length: \
                     {} chars",
                    s.chars().count()
                )));
            }
        }
        // v6.0.1: f32 → SQ8 INSERT-time quantisation. Triggered
        // when the column declares `VECTOR(N) USING SQ8` and
        // the INSERT VALUES expression yields a raw f32 vector
        // (the normal pgvector-shape literal). Dim mismatch
        // falls through the `_ => None` arm and surfaces as
        // `TypeMismatch` with the expected SQ8 column type —
        // matching the F32 path's existing error.
        (
            Value::Vector(v),
            DataType::Vector {
                dim,
                encoding: VecEncoding::Sq8,
            },
        ) if v.len() == dim as usize => Some(Value::Sq8Vector(spg_storage::quantize::quantize(&v))),
        // v6.0.3: f32 → f16 INSERT-time conversion for HALF
        // columns. Bit-exact at the storage layer (modulo
        // half-precision rounding); no rerank pass needed at
        // search time.
        (
            Value::Vector(v),
            DataType::Vector {
                dim,
                encoding: VecEncoding::F16,
            },
        ) if v.len() == dim as usize => Some(Value::HalfVector(
            spg_storage::halfvec::HalfVector::from_f32_slice(&v),
        )),
        // CHAR(n) right-pads with U+0020 to exactly n chars; if the input
        // is already longer we reject (PG truncates trailing-space-only;
        // staying strict for v1).
        (Value::Text(s), DataType::Char(size)) => {
            let len = u32::try_from(s.chars().count()).unwrap_or(u32::MAX);
            if len > size {
                return Err(EngineError::Unsupported(alloc::format!(
                    "value for CHAR({size}) column `{col_name}` exceeds length: \
                     {len} chars"
                )));
            }
            let need = (size - len) as usize;
            let mut padded = s;
            padded.reserve(need);
            for _ in 0..need {
                padded.push(' ');
            }
            Some(Value::Text(padded))
        }
        _ => None,
    };
    coerced.ok_or(EngineError::Storage(StorageError::TypeMismatch {
        column: col_name.into(),
        expected,
        actual,
        position,
    }))
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

/// v7.19 P5 — true iff `expr` is `unnest(arg)` at the top level
/// (case-insensitive). Used by `exec_select_cancel`'s
/// projection loop to detect Set-Returning-Function rows that
/// need per-row expansion. Only the top-level call counts —
/// `coalesce(unnest(arr), 'x')` is NOT a SRF row from the
/// projection's perspective; it would surface as an "unknown
/// function" mismatch downstream, which is what we want
/// (multi-SRF / nested SRF is documented carve-out for v7.19).
fn is_top_level_unnest(expr: &spg_sql::ast::Expr) -> bool {
    match expr {
        spg_sql::ast::Expr::FunctionCall { name, args } => {
            name.eq_ignore_ascii_case("unnest") && args.len() == 1
        }
        _ => false,
    }
}

/// v7.19 P5 — extract the array argument out of a top-level
/// `unnest(arg)` call. `None` if `expr` isn't a `unnest` call
/// of arity 1 (mirrors `is_top_level_unnest`).
fn top_level_unnest_arg(expr: &spg_sql::ast::Expr) -> Option<&spg_sql::ast::Expr> {
    match expr {
        spg_sql::ast::Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("unnest") && args.len() == 1 =>
        {
            Some(&args[0])
        }
        _ => None,
    }
}

/// v7.19 P5 — turn an array-typed `Value` into the element list
/// `unnest()` projection emits. NULL → empty list (PG: `unnest(NULL)
/// = (no rows)`). Non-array values fall through to a type-mismatch
/// error.
fn array_value_to_elements(v: &Value) -> Result<Vec<Value>, EngineError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::TextArray(items) => Ok(items
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|s| Value::Text(s.clone()))
                    .unwrap_or(Value::Null)
            })
            .collect()),
        Value::IntArray(items) => Ok(items
            .iter()
            .map(|opt| opt.map(Value::Int).unwrap_or(Value::Null))
            .collect()),
        Value::BigIntArray(items) => Ok(items
            .iter()
            .map(|opt| opt.map(Value::BigInt).unwrap_or(Value::Null))
            .collect()),
        other => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!(
                "unnest() expects an array argument, got {:?}",
                other.data_type()
            ),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn unwrap_command_ok(r: &QueryResult) -> usize {
        match r {
            QueryResult::CommandOk { affected, .. } => *affected,
            QueryResult::Rows { .. } => panic!("expected CommandOk, got Rows"),
        }
    }

    #[test]
    fn update_seek_positions_engages_on_indexed_eq() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE b (id INT NOT NULL, v INT NOT NULL)")
            .unwrap();
        e.execute("CREATE INDEX b_id ON b (id)").unwrap();
        for i in 0..100 {
            e.execute(&alloc::format!("INSERT INTO b VALUES ({i}, {i})"))
                .unwrap();
        }
        let stmt = spg_sql::parser::parse_statement("UPDATE b SET v = v + 1 WHERE id = 42")
            .expect("parse");
        let Statement::Update(u) = stmt else {
            panic!("expected Update, got {stmt:?}");
        };
        let w = u.where_.as_ref().expect("where");
        let table = e.catalog().get("b").unwrap();
        let schema_cols = table.schema().columns.clone();
        // step-by-step: each sub-resolution must succeed.
        let Expr::Binary { lhs, op, rhs } = w else {
            panic!("WHERE not Binary: {w:?}");
        };
        assert_eq!(*op, BinOp::Eq, "op not Eq");
        let pair = resolve_col_literal_pair(lhs, rhs, &schema_cols, "b");
        assert!(
            pair.is_some(),
            "resolve_col_literal_pair None: lhs={lhs:?} rhs={rhs:?}"
        );
        let (col_pos, value) = pair.unwrap();
        assert!(
            table.index_on(col_pos).is_some(),
            "no index on col {col_pos}"
        );
        assert!(
            IndexKey::from_value(&value).is_some(),
            "IndexKey::from_value None for {value:?}"
        );
        let positions = try_index_seek_positions(w, &schema_cols, table, "b");
        assert_eq!(positions, Some(vec![42]), "seek did not engage");
    }

    #[test]
    fn create_table_registers_schema() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT)")
            .unwrap();
        assert_eq!(e.catalog().table_count(), 1);
        let t = e.catalog().get("foo").unwrap();
        assert_eq!(t.schema().columns.len(), 2);
        assert_eq!(t.schema().columns[0].ty, DataType::Int);
        assert!(!t.schema().columns[0].nullable);
        assert_eq!(t.schema().columns[1].ty, DataType::Text);
    }

    #[test]
    fn create_table_vector_default_is_f32_encoded() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(8))").unwrap();
        let t = e.catalog().get("t").unwrap();
        assert_eq!(
            t.schema().columns[0].ty,
            DataType::Vector {
                dim: 8,
                encoding: VecEncoding::F32,
            },
        );
    }

    #[test]
    fn create_table_vector_using_sq8_succeeds() {
        // v6.0.1 step 3: the step-1 fence in `column_def_to_schema`
        // is lifted. CREATE TABLE persists an SQ8 column type in
        // the catalog; INSERT (next test) quantises raw f32 input.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(8) USING SQ8)").unwrap();
        let t = e.catalog().get("t").unwrap();
        assert_eq!(
            t.schema().columns[0].ty,
            DataType::Vector {
                dim: 8,
                encoding: VecEncoding::Sq8,
            },
        );
    }

    #[test]
    fn insert_into_sq8_column_quantises_f32_payload() {
        // v6.0.1 step 3: INSERT-time `coerce_value` rewrites a raw
        // `Value::Vector(Vec<f32>)` literal into the column's
        // quantised representation. The row that lands in the
        // catalog must therefore hold a `Value::Sq8Vector`, not the
        // original f32 buffer — that's the bit that delivers the
        // 4× compression target.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(4) USING SQ8)").unwrap();
        e.execute("INSERT INTO t VALUES ([0.0, 0.25, 0.5, 1.0])")
            .unwrap();
        let t = e.catalog().get("t").unwrap();
        assert_eq!(t.rows().len(), 1);
        match &t.rows()[0].values[0] {
            Value::Sq8Vector(q) => {
                assert_eq!(q.bytes.len(), 4);
                // min/max are derived from the payload: min=0.0, max=1.0.
                assert!((q.min - 0.0).abs() < 1e-6);
                assert!((q.max - 1.0).abs() < 1e-6);
            }
            other => panic!("expected Sq8Vector cell, got {other:?}"),
        }
    }

    #[test]
    fn create_table_vector_using_half_succeeds_and_insert_converts_to_f16() {
        // v6.0.3: CREATE TABLE accepts USING HALF; INSERT path
        // converts the incoming `Value::Vector(Vec<f32>)` cell
        // into `Value::HalfVector(HalfVector)` via the new
        // `coerce_value` arm. The dequantised round-trip is
        // bit-exact for f16-representable values, so 0.0 / 0.25
        // / 0.5 / 1.0 hit their grid points exactly.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(4) USING HALF)")
            .unwrap();
        e.execute("INSERT INTO t VALUES ([0.0, 0.25, 0.5, 1.0])")
            .unwrap();
        let t = e.catalog().get("t").unwrap();
        assert_eq!(t.rows().len(), 1);
        match &t.rows()[0].values[0] {
            Value::HalfVector(h) => {
                assert_eq!(h.dim(), 4);
                let back = h.to_f32_vec();
                let expected = alloc::vec![0.0_f32, 0.25, 0.5, 1.0];
                for (g, e) in back.iter().zip(expected.iter()) {
                    assert!(
                        (g - e).abs() < 1e-6,
                        "{g} vs {e} should be exact on f16 grid"
                    );
                }
            }
            other => panic!("expected HalfVector cell, got {other:?}"),
        }
    }

    #[test]
    fn alter_index_rebuild_in_place_succeeds() {
        // v6.0.4: bare REBUILD (no encoding switch) walks every
        // row again to rebuild the NSW graph. Verifies the engine
        // dispatch + storage helper plumbing without changing any
        // cell encoding.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(3) NOT NULL)")
            .unwrap();
        for i in 0..8_i32 {
            #[allow(clippy::cast_precision_loss)]
            let base = (i as f32) * 0.1;
            e.execute(&alloc::format!(
                "INSERT INTO t VALUES ({i}, [{base}, {b1}, {b2}])",
                b1 = base + 0.01,
                b2 = base + 0.02,
            ))
            .unwrap();
        }
        e.execute("CREATE INDEX t_idx ON t USING hnsw (v)").unwrap();
        e.execute("ALTER INDEX t_idx REBUILD").unwrap();
        // Schema encoding stays F32 (no encoding clause).
        assert_eq!(
            e.catalog().get("t").unwrap().schema().columns[1].ty,
            DataType::Vector {
                dim: 3,
                encoding: VecEncoding::F32,
            },
        );
    }

    #[test]
    fn alter_index_rebuild_with_encoding_switches_cell_type() {
        // v6.0.4: REBUILD WITH (encoding = SQ8) recodes every
        // stored cell from F32 → SQ8 + rebuilds the graph atop the
        // new encoding. Post-rebuild, cells must be Sq8Vector and
        // the schema must report encoding = Sq8.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(4) NOT NULL)")
            .unwrap();
        e.execute("INSERT INTO t VALUES (1, [0.0, 0.25, 0.5, 1.0])")
            .unwrap();
        e.execute("CREATE INDEX t_idx ON t USING hnsw (v)").unwrap();
        e.execute("ALTER INDEX t_idx REBUILD WITH (encoding = SQ8)")
            .unwrap();
        let t = e.catalog().get("t").unwrap();
        assert_eq!(
            t.schema().columns[1].ty,
            DataType::Vector {
                dim: 4,
                encoding: VecEncoding::Sq8,
            },
        );
        assert!(matches!(t.rows()[0].values[1], Value::Sq8Vector(_)));
    }

    #[test]
    fn alter_index_rebuild_unknown_index_errors() {
        let mut e = Engine::new();
        let err = e.execute("ALTER INDEX nope REBUILD").unwrap_err();
        assert!(
            matches!(
                &err,
                EngineError::Storage(StorageError::IndexNotFound { name }) if name == "nope"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn alter_index_rebuild_on_btree_index_errors() {
        // REBUILD on a B-tree index has no semantic meaning in
        // v6.0.4 — rejected at the storage layer with `Unsupported`.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        e.execute("CREATE INDEX t_idx ON t (id)").unwrap();
        let err = e.execute("ALTER INDEX t_idx REBUILD").unwrap_err();
        assert!(
            matches!(&err, EngineError::Storage(StorageError::Unsupported(_))),
            "got: {err}"
        );
    }

    #[test]
    fn prepared_insert_substitutes_placeholders() {
        // v6.1.1: prepare() parses once; execute_prepared() walks the
        // AST and replaces $1/$2 with the param Values BEFORE the
        // dispatch sees them. Same logical result as a simple-query
        // INSERT, but parse happens once per *statement*, not per
        // execution.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        let stmt = e.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
        for (id, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            e.execute_prepared(stmt.clone(), &[Value::Int(id), Value::Text(name.into())])
                .unwrap();
        }
        // Read back via simple-query SELECT.
        let rows_result = e.execute("SELECT id, name FROM t").unwrap();
        let QueryResult::Rows { rows, .. } = rows_result else {
            panic!("expected Rows")
        };
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn prepared_select_with_placeholder_filters_rows() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
            .unwrap();
        for i in 0..10_i32 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, {})", i * 7))
                .unwrap();
        }
        let stmt = e.prepare("SELECT id FROM t WHERE v = $1").unwrap();
        let QueryResult::Rows { rows, .. } = e.execute_prepared(stmt, &[Value::Int(35)]).unwrap()
        else {
            panic!("expected Rows")
        };
        // v = 35 means i*7 = 35 → i = 5.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int(5));
    }

    #[test]
    fn prepared_too_few_params_errors() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        let stmt = e.prepare("INSERT INTO t VALUES ($1)").unwrap();
        let err = e.execute_prepared(stmt, &[]).unwrap_err();
        assert!(
            matches!(
                &err,
                EngineError::Eval(EvalError::PlaceholderOutOfRange { n: 1, bound: 0 })
            ),
            "got: {err}"
        );
    }

    #[test]
    fn bytea_cast_round_trips_text_input() {
        // v7.18 — `'hello'::bytea` produces the raw bytes. Closes
        // the mailrs D-pre #3 reverse-acceptance gap.
        let e = Engine::new();
        let r = e.execute_readonly("SELECT 'hello'::bytea").unwrap();
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Bytes(b"hello".to_vec()));
    }

    #[test]
    fn bytea_cast_pg_escape_hex_form() {
        // E'\\xdeadbeef'::bytea — E-string decodes to `\xdeadbeef`
        // (literal 10 chars), then ::bytea reads it as PG hex
        // form bytea literal → 4 bytes.
        let e = Engine::new();
        let r = e.execute_readonly(r"SELECT E'\\xdeadbeef'::bytea").unwrap();
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(
            rows[0].values[0],
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );
    }

    #[test]
    fn bytea_cast_chains_through_octet_length() {
        // octet_length('hello'::bytea) → 5. Confirms the cast
        // composes inside larger expressions, not just at top
        // level.
        let e = Engine::new();
        let r = e
            .execute_readonly("SELECT octet_length('hello'::bytea)")
            .unwrap();
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        match &rows[0].values[0] {
            Value::Int(n) => assert_eq!(*n, 5),
            Value::BigInt(n) => assert_eq!(*n, 5),
            other => panic!("expected integer length, got {other:?}"),
        }
    }

    #[test]
    fn readonly_prepared_on_snapshot_select_with_placeholder() {
        // v7.18 — sqlx Pool fan-out relies on running prepared
        // SELECTs against a frozen snapshot without re-entering
        // the writer engine. Mirrors the simple-query SELECT path
        // in `execute_readonly_on_snapshot` but takes a Statement
        // + bound params (the shape sqlx's Execute path produces).
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
            .unwrap();
        for i in 0..10_i32 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, {})", i * 7))
                .unwrap();
        }
        let snapshot = e.clone_snapshot();
        let stmt = e.prepare("SELECT id FROM t WHERE v = $1").unwrap();
        let QueryResult::Rows { rows, .. } =
            Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(35)])
                .unwrap()
        else {
            panic!("expected Rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int(5));
    }

    #[test]
    fn readonly_prepared_on_snapshot_rejects_writes() {
        // DDL / DML prepared statements on the readonly path must
        // surface `WriteRequired` so the spg-sqlx connection layer
        // routes them to the writer mutex instead of the snapshot.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        let snapshot = e.clone_snapshot();
        let stmt = e.prepare("INSERT INTO t VALUES ($1)").unwrap();
        let err = Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(1)])
            .unwrap_err();
        assert!(matches!(&err, EngineError::WriteRequired), "got: {err}");
    }

    #[test]
    fn readonly_prepared_on_snapshot_frozen_view() {
        // The snapshot reflects engine state at clone_snapshot()
        // time. Writes after the snapshot are NOT visible — caller
        // takes a fresh snapshot (or `AsyncReadHandle::refresh()`)
        // to see them. This is the contract the per-statement
        // refresh in spg-sqlx relies on.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        let snapshot = e.clone_snapshot();
        e.execute("INSERT INTO t VALUES (2)").unwrap();
        let stmt = e.prepare("SELECT id FROM t WHERE id = $1").unwrap();
        let QueryResult::Rows { rows, .. } =
            Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(2)])
                .unwrap()
        else {
            panic!("expected Rows")
        };
        assert!(rows.is_empty(), "id=2 was inserted after snapshot");
    }

    #[test]
    fn describe_prepared_on_snapshot_resolves_columns() {
        // v7.18 — sqlx's Executor::describe path on the readonly
        // fan-out needs to resolve column names + types against
        // the snapshot's catalog (not the live engine's catalog,
        // which may have moved on).
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        let snapshot = e.clone_snapshot();
        let stmt = e.prepare("SELECT id, name FROM t WHERE id = $1").unwrap();
        let (_params, cols) = Engine::describe_prepared_on_snapshot(&snapshot, &stmt);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].ty, DataType::Int);
        assert_eq!(cols[1].name, "name");
        assert_eq!(cols[1].ty, DataType::Text);
    }

    #[test]
    fn insert_into_half_column_dim_mismatch_errors() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(4) USING HALF)")
            .unwrap();
        let err = e.execute("INSERT INTO t VALUES ([1.0, 2.0])").unwrap_err();
        assert!(matches!(
            &err,
            EngineError::Storage(StorageError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn insert_into_sq8_column_dim_mismatch_errors() {
        // Dim mismatch falls through the `coerce_value` Vector→Sq8
        // arm's guard and surfaces as `TypeMismatch` — the same
        // error the F32 path produces today, so client error
        // handling stays uniform across encodings.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v VECTOR(4) USING SQ8)").unwrap();
        let err = e.execute("INSERT INTO t VALUES ([1.0, 2.0])").unwrap_err();
        assert!(
            matches!(
                &err,
                EngineError::Storage(StorageError::TypeMismatch { .. })
            ),
            "got: {err}",
        );
    }

    #[test]
    fn create_table_duplicate_errors() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let err = e.execute("CREATE TABLE foo (a INT)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::DuplicateTable { ref name }) if name == "foo"
        ));
    }

    #[test]
    fn insert_into_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("INSERT INTO ghost VALUES (1)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { ref name }) if name == "ghost"
        ));
    }

    #[test]
    fn insert_happy_path_reports_one_affected() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        let r = e.execute("INSERT INTO foo VALUES (42)").unwrap();
        assert_eq!(unwrap_command_ok(&r), 1);
        assert_eq!(e.catalog().get("foo").unwrap().row_count(), 1);
    }

    #[test]
    fn insert_arity_mismatch_propagates() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT, b TEXT)").unwrap();
        let err = e.execute("INSERT INTO foo VALUES (1)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::ArityMismatch { .. })
        ));
    }

    #[test]
    fn insert_negative_integer_via_unary_minus() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        e.execute("INSERT INTO foo VALUES (-7)").unwrap();
        let rows = e.catalog().get("foo").unwrap().rows();
        assert_eq!(rows[0].values[0], Value::Int(-7));
    }

    #[test]
    fn insert_expression_evaluated_against_empty_context() {
        // PG-canonical: INSERT VALUES accepts an arbitrary scalar
        // expression. The engine evaluates against an empty row
        // context — column references would error, but pure
        // arithmetic / function calls are fine.
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        e.execute("INSERT INTO foo VALUES (1 + 2)").unwrap();
        let rows = e.catalog().get("foo").unwrap().rows();
        assert_eq!(rows[0].values[0], Value::Int(3));
    }

    #[test]
    fn select_star_returns_all_rows_in_insertion_order() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT NOT NULL)")
            .unwrap();
        e.execute("INSERT INTO foo VALUES (1, 'one')").unwrap();
        e.execute("INSERT INTO foo VALUES (2, 'two')").unwrap();
        e.execute("INSERT INTO foo VALUES (3, 'three')").unwrap();

        let r = e.execute("SELECT * FROM foo").unwrap();
        let QueryResult::Rows { columns, rows } = r else {
            panic!("expected Rows")
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "a");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1].values,
            vec![Value::Int(2), Value::Text("two".into())]
        );
    }

    #[test]
    fn select_star_on_empty_table_returns_zero_rows() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let r = e.execute("SELECT * FROM foo").unwrap();
        match r {
            QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
            QueryResult::CommandOk { .. } => panic!("expected Rows"),
        }
    }

    // --- v0.4: WHERE + projection ------------------------------------------

    fn make_three_row_users(e: &mut Engine) {
        e.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, score INT)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (1, 'alice', 90)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (2, 'bob', NULL)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (3, 'cara', 70)")
            .unwrap();
    }

    fn unwrap_rows(r: QueryResult) -> (Vec<ColumnSchema>, Vec<Row>) {
        match r {
            QueryResult::Rows { columns, rows } => (columns, rows),
            QueryResult::CommandOk { .. } => panic!("expected Rows"),
        }
    }

    #[test]
    fn where_filter_passes_only_true_rows() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e.execute("SELECT * FROM users WHERE id > 1").unwrap();
        let (_, rows) = unwrap_rows(r);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values[0], Value::Int(2));
        assert_eq!(rows[1].values[0], Value::Int(3));
    }

    #[test]
    fn where_with_null_result_filters_out_row() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        // score is NULL for bob → score > 80 is NULL → row excluded
        let r = e.execute("SELECT * FROM users WHERE score > 80").unwrap();
        let (_, rows) = unwrap_rows(r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[1], Value::Text("alice".into()));
    }

    #[test]
    fn projection_named_columns() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e.execute("SELECT name, score FROM users").unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "name");
        assert_eq!(cols[1].name, "score");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].values,
            vec![Value::Text("alice".into()), Value::Int(90)]
        );
    }

    #[test]
    fn projection_with_column_alias() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e
            .execute("SELECT name AS who FROM users WHERE id = 1")
            .unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols[0].name, "who");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Text("alice".into()));
    }

    #[test]
    fn qualified_column_with_table_alias_resolves() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e
            .execute("SELECT u.id, u.name FROM users AS u WHERE u.id < 3")
            .unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols.len(), 2);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn qualified_column_with_wrong_alias_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("SELECT x.id FROM users AS u").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::UnknownQualifier { ref qualifier }) if qualifier == "x"
        ));
    }

    #[test]
    fn select_unknown_column_errors_in_projection() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("SELECT ghost FROM users").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::ColumnNotFound { ref name }) if name == "ghost"
        ));
    }

    #[test]
    fn where_unknown_column_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e
            .execute("SELECT * FROM users WHERE ghost = 1")
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn expression_projection_evaluates_and_renders() {
        // Compound expressions in the SELECT list are evaluated per row;
        // the output column is typed TEXT, name defaults to the expression.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (a INT NOT NULL)").unwrap();
        e.execute("INSERT INTO t VALUES (3)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT 1 + 2 FROM t").unwrap());
        assert_eq!(rows.len(), 1);
        // The expression evaluates to integer 3; rendered as the cell value
        // (storage::Value::Int(3) since arithmetic kept ints).
        assert_eq!(rows[0].values[0], Value::Int(3));
    }

    #[test]
    fn select_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("SELECT * FROM ghost").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    #[test]
    fn invalid_sql_returns_parse_error() {
        // v4.4: UPDATE is now real SQL, so use a true syntactic
        // garbage payload for the parse-error path.
        let mut e = Engine::new();
        let err = e.execute("THIS_IS_NOT_A_KEYWORD foo bar baz").unwrap_err();
        assert!(matches!(err, EngineError::Parse(_)));
    }

    // --- v0.8 CREATE INDEX + index seek ------------------------------------

    #[test]
    fn create_index_registers_on_table() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        e.execute("CREATE INDEX by_name ON users (name)").unwrap();
        let t = e.catalog().get("users").unwrap();
        assert_eq!(t.indices().len(), 1);
        assert_eq!(t.indices()[0].name, "by_name");
    }

    #[test]
    fn create_index_on_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("CREATE INDEX i ON ghost (a)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    #[test]
    fn create_index_on_unknown_column_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("CREATE INDEX i ON users (ghost)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn select_eq_uses_index_returns_same_rows_as_scan() {
        // Build two engines: one with an index, one without. Same query →
        // same row set (index is a planner optimisation, not a semantic
        // change).
        let mut without = Engine::new();
        make_three_row_users(&mut without);
        let mut with = Engine::new();
        make_three_row_users(&mut with);
        with.execute("CREATE INDEX by_id ON users (id)").unwrap();

        let q = "SELECT * FROM users WHERE id = 2";
        let (_, no_idx_rows) = unwrap_rows(without.execute(q).unwrap());
        let (_, idx_rows) = unwrap_rows(with.execute(q).unwrap());
        assert_eq!(no_idx_rows, idx_rows);
        assert_eq!(idx_rows.len(), 1);
    }

    #[test]
    fn select_eq_with_no_matching_index_value_returns_empty() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        e.execute("CREATE INDEX by_id ON users (id)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT * FROM users WHERE id = 999").unwrap());
        assert_eq!(rows.len(), 0);
    }

    // --- v0.9 transactions -------------------------------------------------

    #[test]
    fn begin_sets_in_transaction_flag() {
        let mut e = Engine::new();
        assert!(!e.in_transaction());
        e.execute("BEGIN").unwrap();
        assert!(e.in_transaction());
    }

    #[test]
    fn double_begin_errors() {
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        let err = e.execute("BEGIN").unwrap_err();
        assert_eq!(err, EngineError::TransactionAlreadyOpen);
    }

    #[test]
    fn commit_without_begin_errors() {
        let mut e = Engine::new();
        let err = e.execute("COMMIT").unwrap_err();
        assert_eq!(err, EngineError::NoActiveTransaction);
    }

    #[test]
    fn rollback_without_begin_errors() {
        let mut e = Engine::new();
        let err = e.execute("ROLLBACK").unwrap_err();
        assert_eq!(err, EngineError::NoActiveTransaction);
    }

    #[test]
    fn commit_applies_shadow_to_committed_catalog() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        e.execute("INSERT INTO t VALUES (2)").unwrap();
        e.execute("COMMIT").unwrap();
        assert!(!e.in_transaction());
        assert_eq!(e.catalog().get("t").unwrap().row_count(), 2);
    }

    #[test]
    fn rollback_discards_shadow() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        e.execute("INSERT INTO t VALUES (2)").unwrap();
        e.execute("ROLLBACK").unwrap();
        assert!(!e.in_transaction());
        assert_eq!(e.catalog().get("t").unwrap().row_count(), 0);
    }

    #[test]
    fn select_during_tx_sees_uncommitted_writes_own_session() {
        // The shadow catalog is read by SELECTs while a TX is open — the
        // session can see its own pending writes.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (42)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT * FROM t").unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int(42));
    }

    #[test]
    fn snapshot_with_no_users_is_bare_catalog_format() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        let bytes = e.snapshot();
        assert_eq!(
            &bytes[..8],
            b"SPGDB001",
            "must be the bare v3.x catalog magic"
        );
        let e2 = Engine::restore_envelope(&bytes).unwrap();
        assert!(e2.users().is_empty());
        assert_eq!(e2.catalog().table_count(), 1);
    }

    #[test]
    fn snapshot_with_users_round_trips_both_via_envelope() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        e.create_user("alice", "pw1", Role::Admin, [9; 16]).unwrap();
        e.create_user("bob", "pw2", Role::ReadOnly, [5; 16])
            .unwrap();
        let bytes = e.snapshot();
        assert_eq!(&bytes[..8], b"SPGENV01", "must be the v4.1 envelope magic");
        let e2 = Engine::restore_envelope(&bytes).unwrap();
        assert_eq!(e2.users().len(), 2);
        assert_eq!(e2.verify_user("alice", "pw1"), Some(Role::Admin));
        assert_eq!(e2.verify_user("bob", "pw2"), Some(Role::ReadOnly));
        assert_eq!(e2.verify_user("alice", "wrong"), None);
        assert_eq!(e2.catalog().table_count(), 1);
    }

    #[test]
    fn ddl_inside_tx_also_rolled_back() {
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        e.execute("CREATE TABLE t (v INT)").unwrap();
        // Visible inside the TX.
        e.execute("SELECT * FROM t").unwrap();
        e.execute("ROLLBACK").unwrap();
        // Gone after rollback.
        let err = e.execute("SELECT * FROM t").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    // ── v6.1.2: CREATE / DROP PUBLICATION (engine-side) ──────

    #[test]
    fn create_publication_lands_in_catalog() {
        let mut e = Engine::new();
        assert!(e.publications().is_empty());
        e.execute("CREATE PUBLICATION pub_a").unwrap();
        assert_eq!(e.publications().len(), 1);
        assert!(e.publications().contains("pub_a"));
    }

    #[test]
    fn create_publication_duplicate_errors() {
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION pub_a").unwrap();
        let err = e.execute("CREATE PUBLICATION pub_a").unwrap_err();
        assert!(
            alloc::format!("{err:?}").contains("DuplicateName"),
            "got {err:?}"
        );
    }

    #[test]
    fn drop_publication_silent_when_absent() {
        let mut e = Engine::new();
        // PG-compatible: DROP a publication that doesn't exist
        // succeeds (no-op) but reports zero affected.
        let r = e.execute("DROP PUBLICATION nope").unwrap();
        match r {
            QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 0),
            other => panic!("expected CommandOk, got {other:?}"),
        }
    }

    #[test]
    fn drop_publication_present_reports_one_affected() {
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION pub_a").unwrap();
        let r = e.execute("DROP PUBLICATION pub_a").unwrap();
        match r {
            QueryResult::CommandOk {
                affected,
                modified_catalog,
            } => {
                assert_eq!(affected, 1);
                assert!(modified_catalog);
            }
            other => panic!("expected CommandOk, got {other:?}"),
        }
        assert!(e.publications().is_empty());
    }

    #[test]
    fn publications_persist_across_snapshot_restore() {
        // The persist-across-restart ship-gate at the engine layer —
        // snapshot → restore_envelope round trip must preserve the
        // publication catalog. The spg-server e2e covers the
        // process-restart variant.
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION pub_a").unwrap();
        e.execute("CREATE PUBLICATION pub_b FOR ALL TABLES")
            .unwrap();
        let snap = e.snapshot();
        let e2 = Engine::restore_envelope(&snap).unwrap();
        assert_eq!(e2.publications().len(), 2);
        assert!(e2.publications().contains("pub_a"));
        assert!(e2.publications().contains("pub_b"));
    }

    #[test]
    fn create_publication_allowed_inside_transaction() {
        // v6.1.4 dropped the v6.1.2 in-TX guard — PG allows
        // CREATE PUBLICATION inside a TX and the auto-commit
        // wrap path needs the same allowance.
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        e.execute("CREATE PUBLICATION pub_a").unwrap();
        e.execute("COMMIT").unwrap();
        assert!(e.publications().contains("pub_a"));
    }

    // ── v6.1.3: SHOW PUBLICATIONS + FOR-list variants ───────

    #[test]
    fn create_publication_for_table_list_lands_with_scope() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t1 (id INT NOT NULL)").unwrap();
        e.execute("CREATE TABLE t2 (id INT NOT NULL)").unwrap();
        e.execute("CREATE PUBLICATION pub_a FOR TABLE t1, t2")
            .unwrap();
        let scope = e.publications().get("pub_a").cloned();
        let Some(spg_sql::ast::PublicationScope::ForTables(ts)) = scope else {
            panic!("expected ForTables scope, got {scope:?}")
        };
        assert_eq!(ts, alloc::vec!["t1".to_string(), "t2".to_string()]);
    }

    #[test]
    fn create_publication_all_tables_except_lands_with_scope() {
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION pub_a FOR ALL TABLES EXCEPT t3")
            .unwrap();
        let scope = e.publications().get("pub_a").cloned();
        let Some(spg_sql::ast::PublicationScope::AllTablesExcept(ts)) = scope else {
            panic!("expected AllTablesExcept scope, got {scope:?}")
        };
        assert_eq!(ts, alloc::vec!["t3".to_string()]);
    }

    #[test]
    fn show_publications_empty_returns_zero_rows() {
        let e = Engine::new();
        let r = e.execute_readonly("SHOW PUBLICATIONS").unwrap();
        let QueryResult::Rows { rows, columns } = r else {
            panic!()
        };
        assert!(rows.is_empty());
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].name, "name");
        assert_eq!(columns[1].name, "scope");
        assert_eq!(columns[2].name, "table_count");
    }

    #[test]
    fn show_publications_returns_one_row_per_publication_ordered_by_name() {
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION z_pub").unwrap();
        e.execute("CREATE PUBLICATION a_pub FOR TABLE t1, t2")
            .unwrap();
        e.execute("CREATE PUBLICATION m_pub FOR ALL TABLES EXCEPT bad")
            .unwrap();
        let r = e.execute_readonly("SHOW PUBLICATIONS").unwrap();
        let QueryResult::Rows { rows, .. } = r else {
            panic!()
        };
        assert_eq!(rows.len(), 3);
        // Alphabetical order: a_pub, m_pub, z_pub.
        let names: Vec<&str> = rows
            .iter()
            .map(|r| {
                if let Value::Text(s) = &r.values[0] {
                    s.as_str()
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(names, alloc::vec!["a_pub", "m_pub", "z_pub"]);
        // Row 0 — a_pub scope summary + table_count = 2.
        match &rows[0].values[1] {
            Value::Text(s) => assert_eq!(s, "FOR TABLE t1, t2"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(rows[0].values[2], Value::Int(2));
        // Row 1 — m_pub.
        match &rows[1].values[1] {
            Value::Text(s) => assert_eq!(s, "FOR ALL TABLES EXCEPT bad"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(rows[1].values[2], Value::Int(1));
        // Row 2 — z_pub (AllTables → NULL count).
        match &rows[2].values[1] {
            Value::Text(s) => assert_eq!(s, "FOR ALL TABLES"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(rows[2].values[2], Value::Null);
    }

    #[test]
    fn for_list_scopes_persist_across_snapshot() {
        // The v6.1.2 envelope-v3 round-trip exercised AllTables;
        // v6.1.3 needs the scope-1 / scope-2 tags to survive too.
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION p1 FOR TABLE t1, t2").unwrap();
        e.execute("CREATE PUBLICATION p2 FOR ALL TABLES EXCEPT bad, worse")
            .unwrap();
        let snap = e.snapshot();
        let e2 = Engine::restore_envelope(&snap).unwrap();
        assert_eq!(e2.publications().len(), 2);
        let p1 = e2.publications().get("p1").cloned();
        let Some(spg_sql::ast::PublicationScope::ForTables(ts)) = p1 else {
            panic!("p1 scope lost: {p1:?}")
        };
        assert_eq!(ts, alloc::vec!["t1".to_string(), "t2".to_string()]);
        let p2 = e2.publications().get("p2").cloned();
        let Some(spg_sql::ast::PublicationScope::AllTablesExcept(ts)) = p2 else {
            panic!("p2 scope lost: {p2:?}")
        };
        assert_eq!(ts, alloc::vec!["bad".to_string(), "worse".to_string()]);
    }

    // ── v6.1.4: CREATE / DROP SUBSCRIPTION + SHOW + envelope v4 ─

    #[test]
    fn create_subscription_lands_in_catalog_with_defaults() {
        let mut e = Engine::new();
        e.execute(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host=127.0.0.1 port=20002' PUBLICATION pub_a",
        )
        .unwrap();
        let s = e.subscriptions().get("sub_a").cloned().expect("present");
        assert_eq!(s.conn_str, "host=127.0.0.1 port=20002");
        assert_eq!(s.publications, alloc::vec!["pub_a".to_string()]);
        assert!(s.enabled);
        assert_eq!(s.last_received_pos, 0);
    }

    #[test]
    fn create_subscription_duplicate_name_errors() {
        let mut e = Engine::new();
        e.execute("CREATE SUBSCRIPTION s CONNECTION 'host=x' PUBLICATION p")
            .unwrap();
        let err = e
            .execute("CREATE SUBSCRIPTION s CONNECTION 'host=y' PUBLICATION p")
            .unwrap_err();
        assert!(
            alloc::format!("{err:?}").contains("DuplicateName"),
            "got {err:?}"
        );
    }

    #[test]
    fn drop_subscription_silent_when_absent() {
        let mut e = Engine::new();
        let r = e.execute("DROP SUBSCRIPTION never").unwrap();
        match r {
            QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 0),
            other => panic!("expected CommandOk, got {other:?}"),
        }
    }

    #[test]
    fn subscription_advance_updates_last_pos_monotone() {
        let mut e = Engine::new();
        e.execute("CREATE SUBSCRIPTION s CONNECTION 'h=x' PUBLICATION p")
            .unwrap();
        assert!(e.subscription_advance("s", 100));
        assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 100);
        assert!(e.subscription_advance("s", 50)); // stale → ignored
        assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 100);
        assert!(e.subscription_advance("s", 200));
        assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 200);
        assert!(!e.subscription_advance("missing", 1));
    }

    #[test]
    fn show_subscriptions_returns_rows_ordered_by_name() {
        let mut e = Engine::new();
        e.execute("CREATE SUBSCRIPTION z_sub CONNECTION 'h=x' PUBLICATION p1, p2")
            .unwrap();
        e.execute("CREATE SUBSCRIPTION a_sub CONNECTION 'h=y' PUBLICATION p3")
            .unwrap();
        let r = e.execute_readonly("SHOW SUBSCRIPTIONS").unwrap();
        let QueryResult::Rows { rows, columns } = r else {
            panic!()
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(columns.len(), 5);
        assert_eq!(columns[0].name, "name");
        assert_eq!(columns[4].name, "last_received_pos");
        // Alphabetical: a_sub, z_sub.
        let names: Vec<&str> = rows
            .iter()
            .map(|r| {
                if let Value::Text(s) = &r.values[0] {
                    s.as_str()
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(names, alloc::vec!["a_sub", "z_sub"]);
        // Row 0: a_sub
        assert_eq!(rows[0].values[1], Value::Text("h=y".to_string()));
        assert_eq!(rows[0].values[2], Value::Text("p3".to_string()));
        assert_eq!(rows[0].values[3], Value::Bool(true));
        assert_eq!(rows[0].values[4], Value::BigInt(0));
        // Row 1: z_sub — publications join with ", "
        assert_eq!(rows[1].values[2], Value::Text("p1, p2".to_string()));
    }

    #[test]
    fn subscriptions_persist_across_snapshot_envelope_v4() {
        let mut e = Engine::new();
        e.execute("CREATE SUBSCRIPTION s1 CONNECTION 'h=A' PUBLICATION p1, p2")
            .unwrap();
        e.execute("CREATE SUBSCRIPTION s2 CONNECTION 'h=B' PUBLICATION p3")
            .unwrap();
        e.subscription_advance("s2", 42);
        let snap = e.snapshot();
        let e2 = Engine::restore_envelope(&snap).unwrap();
        assert_eq!(e2.subscriptions().len(), 2);
        let s1 = e2.subscriptions().get("s1").unwrap();
        assert_eq!(s1.conn_str, "h=A");
        assert_eq!(
            s1.publications,
            alloc::vec!["p1".to_string(), "p2".to_string()]
        );
        assert_eq!(s1.last_received_pos, 0);
        let s2 = e2.subscriptions().get("s2").unwrap();
        assert_eq!(s2.last_received_pos, 42);
    }

    #[test]
    fn v3_envelope_loads_with_empty_subscriptions() {
        // v3 snapshot (publications-only). Forge it by hand so we
        // verify v6.1.4 readers don't panic — they must surface
        // empty subscriptions and a populated publication table.
        let mut e = Engine::new();
        e.execute("CREATE PUBLICATION pub_legacy").unwrap();
        let catalog = e.catalog.serialize();
        let users = crate::users::serialize_users(&e.users);
        let pubs = e.publications.serialize();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"SPGENV01");
        buf.push(3u8); // v3
        buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&catalog);
        buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&users);
        buf.extend_from_slice(&u32::try_from(pubs.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&pubs);
        let crc = spg_crypto::crc32::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let e2 = Engine::restore_envelope(&buf).expect("v3 envelope restores under v4 reader");
        assert!(e2.subscriptions().is_empty());
        assert!(e2.publications().contains("pub_legacy"));
    }

    #[test]
    fn create_subscription_allowed_inside_transaction() {
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        e.execute("CREATE SUBSCRIPTION s CONNECTION 'h=x' PUBLICATION p")
            .unwrap();
        e.execute("COMMIT").unwrap();
        assert!(e.subscriptions().contains("s"));
    }

    // ── v6.2.0: ANALYZE + spg_statistic + envelope v5 ──────────
    #[test]
    fn analyze_populates_histogram_bounds() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
            .unwrap();
        for i in 0..50 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'name{i}')"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        let stats = e.statistics();
        let id_stats = stats.get("t", "id").unwrap();
        assert!(id_stats.histogram_bounds.len() >= 2);
        assert_eq!(id_stats.histogram_bounds.first().unwrap(), "0");
        assert_eq!(id_stats.histogram_bounds.last().unwrap(), "49");
        assert!((id_stats.null_frac - 0.0).abs() < 1e-6);
        assert_eq!(id_stats.n_distinct, 50);
    }

    #[test]
    fn reanalyze_overwrites_prior_stats() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..10 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        let n1 = e.statistics().get("t", "id").unwrap().n_distinct;
        assert_eq!(n1, 10);
        for i in 10..30 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        let n2 = e.statistics().get("t", "id").unwrap().n_distinct;
        assert_eq!(n2, 30);
    }

    #[test]
    fn analyze_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("ANALYZE nonexistent").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    #[test]
    fn bare_analyze_covers_all_user_tables() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t1 (id INT NOT NULL)").unwrap();
        e.execute("CREATE TABLE t2 (name TEXT NOT NULL)").unwrap();
        e.execute("INSERT INTO t1 VALUES (1)").unwrap();
        e.execute("INSERT INTO t2 VALUES ('alice')").unwrap();
        let r = e.execute("ANALYZE").unwrap();
        match r {
            QueryResult::CommandOk {
                affected,
                modified_catalog,
            } => {
                assert_eq!(affected, 2);
                assert!(modified_catalog);
            }
            other => panic!("expected CommandOk, got {other:?}"),
        }
        assert!(e.statistics().get("t1", "id").is_some());
        assert!(e.statistics().get("t2", "name").is_some());
    }

    #[test]
    fn select_from_spg_statistic_returns_rows_per_column() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT)")
            .unwrap();
        e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        e.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        e.execute("ANALYZE t").unwrap();
        let r = e.execute_readonly("SELECT * FROM spg_statistic").unwrap();
        let QueryResult::Rows { rows, columns } = r else {
            panic!()
        };
        // v6.7.0 — spg_statistic gained a `cold_row_count` column.
        assert_eq!(columns.len(), 6);
        assert_eq!(columns[0].name, "table_name");
        assert_eq!(columns[4].name, "histogram_bounds");
        assert_eq!(columns[5].name, "cold_row_count");
        assert_eq!(rows.len(), 2, "one row per column of t");
        // Sorted by (table_name, column_name).
        match (&rows[0].values[0], &rows[0].values[1]) {
            (Value::Text(t), Value::Text(c)) => {
                assert_eq!(t, "t");
                // BTreeMap orders (table, column); columns "id" < "label".
                assert_eq!(c, "id");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn analyze_skips_vector_columns() {
        // Vector columns have their own stats shape (HNSW graph);
        // ANALYZE leaves them out of spg_statistic.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(3) NOT NULL)")
            .unwrap();
        e.execute("INSERT INTO t VALUES (1, [1, 2, 3])").unwrap();
        e.execute("ANALYZE t").unwrap();
        assert!(e.statistics().get("t", "id").is_some());
        assert!(e.statistics().get("t", "v").is_none());
    }

    #[test]
    fn statistics_persist_across_envelope_v5_round_trip() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..20 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        e.execute("ANALYZE").unwrap();
        let snap = e.snapshot();
        let e2 = Engine::restore_envelope(&snap).unwrap();
        let s = e2.statistics().get("t", "id").unwrap();
        assert_eq!(s.n_distinct, 20);
    }

    // ── v6.2.1 auto-analyze threshold ───────────────────────────

    #[test]
    fn auto_analyze_threshold_fires_after_10pct_of_min_rows_on_small_table() {
        // For a table with 0 rows then 10 inserts → modified=10,
        // row_count=10. Threshold = 0.1 × max(10, 100) = 10. So
        // after the 10th INSERT the threshold is met.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..9 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        assert!(e.tables_needing_analyze().is_empty(), "9 < threshold");
        e.execute("INSERT INTO t VALUES (9)").unwrap();
        let needs = e.tables_needing_analyze();
        assert_eq!(needs, alloc::vec!["t".to_string()]);
    }

    #[test]
    fn auto_analyze_threshold_uses_10pct_of_row_count_for_large_tables() {
        // After ANALYZE on 1000 rows, threshold = 0.1 × row_count.
        // Each new INSERT bumps both modified and row_count, so to
        // trigger from N=1000 we need modifications ≥ 0.1 × (1000+M),
        // i.e. M ≥ 112. The test inserts 50 (no fire), then 150
        // more (200 total mods, row_count=1200, threshold=120 → fire).
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..1000 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        assert!(e.tables_needing_analyze().is_empty(), "fresh ANALYZE");
        for i in 1000..1050 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        assert!(
            e.tables_needing_analyze().is_empty(),
            "50 inserts < threshold of ~105"
        );
        for i in 1050..1200 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        assert_eq!(
            e.tables_needing_analyze(),
            alloc::vec!["t".to_string()],
            "200 inserts > 0.1 × 1200 threshold"
        );
    }

    #[test]
    fn auto_analyze_threshold_resets_after_analyze() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..200 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        assert!(!e.tables_needing_analyze().is_empty());
        e.execute("ANALYZE").unwrap();
        assert!(
            e.tables_needing_analyze().is_empty(),
            "ANALYZE must reset the counter"
        );
    }

    #[test]
    fn auto_analyze_threshold_tracks_updates_and_deletes() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT)")
            .unwrap();
        for i in 0..50 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'x')"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        // UPDATE 20 rows + DELETE 5 → modified=25. Threshold = 0.1
        // × max(50, 100) = 10. So 25 >= 10 → trigger.
        e.execute("UPDATE t SET label = 'y' WHERE id < 20").unwrap();
        e.execute("DELETE FROM t WHERE id >= 45").unwrap();
        assert_eq!(e.tables_needing_analyze(), alloc::vec!["t".to_string()]);
    }

    #[test]
    fn v4_envelope_loads_with_empty_statistics() {
        // Forge a v4 envelope by hand: catalog + users + pubs +
        // subs trailer, no statistics. A v6.2.0 reader must accept
        // it and surface an empty Statistics.
        let mut e = Engine::new();
        e.create_user("alice", "secret", crate::users::Role::ReadOnly, [0u8; 16])
            .unwrap();
        let catalog = e.catalog.serialize();
        let users = crate::users::serialize_users(&e.users);
        let pubs = e.publications.serialize();
        let subs = e.subscriptions.serialize();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"SPGENV01");
        buf.push(4u8);
        buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&catalog);
        buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&users);
        buf.extend_from_slice(&u32::try_from(pubs.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&pubs);
        buf.extend_from_slice(&u32::try_from(subs.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&subs);
        let crc = spg_crypto::crc32::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        let e2 = Engine::restore_envelope(&buf).expect("v4 envelope restores");
        assert!(e2.statistics().is_empty());
    }

    #[test]
    fn v1_v2_envelope_loads_with_empty_publications() {
        // A snapshot taken before v6.1.2 (no publication trailer,
        // envelope v2) must still deserialise — and the resulting
        // engine must report zero publications. Use the engine's own
        // round-trip with no publications: that emits v3 but with an
        // empty pubs block. Then forge a v2 envelope by hand to lock
        // the back-compat path.
        let mut e = Engine::new();
        // Force users to be non-empty so the snapshot takes the
        // envelope path rather than the bare-catalog fallback.
        e.create_user("alice", "secret", crate::users::Role::ReadOnly, [0u8; 16])
            .unwrap();

        // Forge an envelope v2: same shape as v3 but no pubs trailer.
        let catalog = e.catalog.serialize();
        let users = crate::users::serialize_users(&e.users);
        let mut buf = Vec::new();
        buf.extend_from_slice(b"SPGENV01");
        buf.push(2u8); // v2
        buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&catalog);
        buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&users);
        let crc = spg_crypto::crc32::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let e2 = Engine::restore_envelope(&buf).expect("v2 envelope restores");
        assert!(e2.publications().is_empty());
    }
}
