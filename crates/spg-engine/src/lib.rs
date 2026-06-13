//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
mod conversions;
pub mod copy;
mod ddl;
pub mod describe;
mod dml;
pub mod eval;
mod explain;
mod expr_analysis;
pub mod fts;
mod index_access;
mod join;
pub mod json;
pub mod memoize;
mod numeric;
pub mod plan_cache;
pub mod publications;
pub mod query_stats;
pub mod reorder;
mod select;
pub mod selectivity;
pub mod statistics;
pub mod subscriptions;
mod system_catalog;
mod table_access;
mod transaction;
pub mod triggers;
pub mod users;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};

use conversions::*;
pub use conversions::{
    format_bigint_2d_text_pub, format_hstore_text, format_int_2d_text_pub, format_range_text,
    format_text_2d_text_pub,
};
use explain::*;
use expr_analysis::*;
use index_access::*;
use system_catalog::*;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, CreatePublicationStatement, CreateSubscriptionStatement, Expr,
    FrameBound, FrameKind, Literal, OrderBy, SelectItem, SelectStatement, Statement, WindowFrame,
};
// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::Statement as ParsedStatement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{
    Catalog, ColumnSchema, CompactReport, DataType, IndexKind, Row, StorageError, Value,
    VecEncoding,
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

    pub(crate) fn active_catalog(&self) -> &Catalog {
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
    /// v7.31 (memory campaign) — walk the committed catalog and
    /// build the per-bucket memory snapshot. O(rows + index
    /// entries): operator/monitoring surface, not a query path.
    pub fn memory_stats(&self) -> MemoryStats {
        let mut tables: Vec<TableMemoryStats> = Vec::new();
        let (mut total_enc, mut total_res, mut total_idx) = (0u64, 0u64, 0u64);
        for tname in self.catalog.table_names() {
            if is_internal_table_name(&tname) {
                continue;
            }
            let Some(t) = self.catalog.get(&tname) else {
                continue;
            };
            let resident: u64 = t.rows().iter().map(|r| approx_row_bytes(r) as u64).sum();
            let mut idx_bytes: u64 = 0;
            for idx in t.indices() {
                idx_bytes += match &idx.kind {
                    spg_storage::IndexKind::BTree(map) => {
                        let mut b: u64 = 0;
                        for (_, locs) in map.iter() {
                            b += (core::mem::size_of::<spg_storage::IndexKey>()
                                + 24
                                + locs.len() * core::mem::size_of::<spg_storage::RowLocator>())
                                as u64;
                        }
                        b
                    }
                    // Parametric estimate: per node, the dense
                    // layer-0 neighbour list dominates.
                    spg_storage::IndexKind::Nsw(g) => {
                        (g.levels.len() * (g.m_max_0 * 8 + 16)) as u64
                    }
                    // BRIN is block-range metadata — flat token.
                    _ => 1024,
                };
            }
            total_enc += t.hot_bytes();
            total_res += resident;
            total_idx += idx_bytes;
            tables.push(TableMemoryStats {
                name: tname.clone(),
                hot_rows: t.rows().len() as u64,
                cold_rows: t.cold_row_count(),
                hot_encoded_bytes: t.hot_bytes(),
                approx_resident_bytes: resident,
                index_count: t.indices().len() as u64,
                approx_index_bytes: idx_bytes,
            });
        }
        MemoryStats {
            tables,
            total_hot_encoded_bytes: total_enc,
            total_approx_resident_bytes: total_res,
            total_approx_index_bytes: total_idx,
            max_query_bytes: self.max_query_bytes,
        }
    }

    /// v7.31 — `SELECT * FROM spg_memory_stats`: one row per user
    /// table (same numbers as `Engine::memory_stats()`), so the
    /// server path gets the meter through plain SQL.
    fn exec_spg_memory_stats(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("hot_rows", DataType::BigInt, false),
            ColumnSchema::new("cold_rows", DataType::BigInt, false),
            ColumnSchema::new("hot_encoded_bytes", DataType::BigInt, false),
            ColumnSchema::new("approx_resident_bytes", DataType::BigInt, false),
            ColumnSchema::new("index_count", DataType::BigInt, false),
            ColumnSchema::new("approx_index_bytes", DataType::BigInt, false),
        ];
        #[allow(clippy::cast_possible_wrap)]
        let rows: Vec<Row> = self
            .memory_stats()
            .tables
            .into_iter()
            .map(|t| {
                Row::new(alloc::vec![
                    Value::Text(t.name),
                    Value::BigInt(t.hot_rows as i64),
                    Value::BigInt(t.cold_rows as i64),
                    Value::BigInt(t.hot_encoded_bytes as i64),
                    Value::BigInt(t.approx_resident_bytes as i64),
                    Value::BigInt(t.index_count as i64),
                    Value::BigInt(t.approx_index_bytes as i64),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

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

/// v7.30.3 (mailrs round-26) — approximate heap bytes held by one
/// `Value`. Fat payloads (text / json / bytea / vectors / arrays)
/// dominate; fixed-size variants count 0 here because the per-cell
/// enum overhead is charged separately in `approx_row_bytes`. An
/// under-estimate is acceptable — the budget is a host-pressure
/// guard, not an exact meter.
pub(crate) fn approx_value_bytes(v: &Value) -> usize {
    match v {
        Value::Text(s) | Value::Json(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Vector(v) => v.len() * 4,
        Value::TextArray(a) => a
            .iter()
            .map(|o| o.as_ref().map_or(0, String::len) + 8)
            .sum(),
        Value::IntArray(a) => a.len() * 8,
        _ => 0,
    }
}

/// Approximate heap bytes held by one materialised `Row`: per-cell
/// enum slots plus fat payloads.
fn approx_row_bytes(row: &Row) -> usize {
    row.values.len() * core::mem::size_of::<Value>()
        + row.values.iter().map(approx_value_bytes).sum::<usize>()
}

/// v7.30.3 (mailrs round-26) — per-query byte budget for join/filter
/// materialisation. Net accounting: stages charge what they clone and
/// release what they free (`working` is released when the next stage
/// replaces it), so the meter tracks live bytes, not cumulative
/// churn. `limit = usize::MAX` when the budget is disabled keeps the
/// hot path branch-free apart from one saturating add + compare.
struct ByteBudget {
    limit: usize,
    used: usize,
}

impl ByteBudget {
    const fn new(limit: Option<usize>) -> Self {
        Self {
            limit: match limit {
                Some(n) => n,
                None => usize::MAX,
            },
            used: 0,
        }
    }

    fn charge(&mut self, n: usize) -> Result<(), EngineError> {
        self.used = self.used.saturating_add(n);
        if self.used > self.limit {
            return Err(EngineError::QueryBytesExceeded(self.limit));
        }
        Ok(())
    }

    fn release(&mut self, n: usize) {
        self.used = self.used.saturating_sub(n);
    }
}

/// Sum `approx_row_bytes` over a freshly materialised row set.
fn approx_rows_bytes(rows: &[Row]) -> usize {
    rows.iter().map(approx_row_bytes).sum()
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

pub(crate) fn build_projection(
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
    /// v4.23: per-row eval that handles correlated subqueries.
    /// Equivalent to `eval::eval_expr` when the expression has no
    /// subqueries; otherwise clones the expression, substitutes
    /// outer-row columns into each surviving subquery node, runs
    /// the inner SELECT, and replaces the node with the literal
    /// result. Only the WHERE-filter call sites use this path so
    /// the uncorrelated fast path is preserved everywhere else.
    pub(crate) fn eval_expr_with_correlated(
        &self,
        expr: &Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<Value, EngineError> {
        // v7.30.2 (mailrs round-25) — the has-subquery walk is
        // O(tree) and a materialised `IN (…)` list makes the tree
        // huge; cache the answer per expression address so the
        // per-row dispatch stops re-walking 24k list elements.
        let has_subq = if let Some(m) = memo.as_deref_mut() {
            let key = core::ptr::from_ref::<Expr>(expr) as usize;
            match m.has_subquery.get(&key) {
                Some(b) => *b,
                None => {
                    let b = expr_has_subquery(expr);
                    m.has_subquery.insert(key, b);
                    b
                }
            }
        } else {
            expr_has_subquery(expr)
        };
        if !has_subq {
            // A large materialised `IN (…)` list inside the WHERE
            // makes the plain eval O(rows × list); route through the
            // per-query membership set (built once, keyed by node
            // address) when one is reachable on the AND spine.
            if let Some(m) = memo.as_deref_mut()
                && expr_may_use_in_set(expr)
            {
                return eval_with_in_sets(expr, row, ctx, m);
            }
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
                            .try_batch_correlated_scalar(sub, None, cancel)?
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
                            .try_batch_correlated_scalar(inner, None, cancel)?
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
            Expr::InList { expr, list, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                for item in list {
                    self.resolve_correlated_in_expr(item, row, ctx, cancel, memo.as_deref_mut())?;
                }
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
                // v7.32 (R30) — a correlated subquery is resolved by
                // the per-row / post-LIMIT correlated path; executing
                // it here only to catch the correlation error first
                // materialises (and discards) its whole inner FROM.
                if select_is_correlated(inner) {
                    return Ok(None);
                }
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
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
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
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
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
                // v7.30.2 (mailrs round-25) — flat InList, NOT an OR-Eq
                // chain: chain depth scaled with the inner result's ROW
                // COUNT, so one 24k-match search overflowed the worker
                // stack (recursive eval + recursive Box drop) and
                // aborted the embedding host process.
                let mut list: Vec<Expr> = Vec::with_capacity(rows.len());
                for row in rows {
                    let v = row.values.into_iter().next().unwrap_or(Value::Null);
                    list.push(value_to_literal_expr(v)?);
                }
                Ok(Some(Expr::InList {
                    expr: expr.clone(),
                    list,
                    negated: *negated,
                }))
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

/// v4.22: pick more specific column types from observed rows when
/// the projection builder defaulted to Text (the v1.x behavior for
/// non-column expressions). Lets `WITH t(n) AS (SELECT 1 ...)`
/// land an Int column in the CTE storage table rather than failing
/// the insert with "expected TEXT, got INT".
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

/// v7.32 (R30 memory) — cheap static correlation pre-check.
///
/// `subquery_replacement` distinguishes a correlated subquery from an
/// uncorrelated one by *optimistically executing* it and catching the
/// resulting `ColumnNotFound` / `UnknownQualifier`. For a join-bodied
/// correlated subquery that catch fires only AFTER the inner FROM is
/// materialised — and the deferred-join pipeline clones the whole
/// driving table to do it (the inbox `… JOIN messages m2 …` body
/// clones 960k × 10 KB ≈ 10 GB at prod scale, once per outer query,
/// purely to be thrown away). A correlated subquery is always handled
/// downstream by the per-row / post-LIMIT correlated path, so spotting
/// it up front lets us skip the wasted materialisation entirely.
///
/// Sound for the `true` answer: returns true only when a qualified
/// column at the statement's own level names a qualifier that is not
/// one of its own FROM aliases — exactly the reference the inner exec
/// would fail to resolve. Everything it can't reason about cleanly
/// (lateral / derived FROM entries) returns false and falls through to
/// the existing execute-and-catch path, so behaviour is unchanged.
fn select_is_correlated(s: &SelectStatement) -> bool {
    use spg_sql::ast::SelectItem;
    let Some(from) = &s.from else {
        // No FROM: correlated iff some projected column is qualified
        // (a qualifier with nothing to bind to is necessarily outer).
        let mut qualified = false;
        for item in &s.items {
            if let SelectItem::Expr { expr, .. } = item {
                visit_expr_columns_and_subqueries(
                    expr,
                    &mut |c| {
                        if c.qualifier.is_some() {
                            qualified = true;
                        }
                    },
                    &mut |_| {},
                );
            }
        }
        return qualified;
    };
    // Lateral / derived FROM entries put scope resolution beyond this
    // cheap check — defer to execute-and-catch.
    if from.primary.lateral_subquery.is_some() {
        return false;
    }
    let mut inner: Vec<&str> = Vec::new();
    if let Some(a) = &from.primary.alias {
        inner.push(a.as_str());
    }
    if !from.primary.name.is_empty() {
        inner.push(from.primary.name.as_str());
    }
    for j in &from.joins {
        if j.table.lateral_subquery.is_some() {
            return false;
        }
        if let Some(a) = &j.table.alias {
            inner.push(a.as_str());
        }
        if !j.table.name.is_empty() {
            inner.push(j.table.name.as_str());
        }
    }
    // Gather every expression position that evaluates in this
    // statement's own scope (NOT inside nested subquery bodies — the
    // visitor reports those via the subquery callback, which we drop).
    let mut exprs: Vec<&Expr> = Vec::new();
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            exprs.push(expr);
        }
    }
    if let Some(w) = &s.where_ {
        exprs.push(w);
    }
    for j in &from.joins {
        if let Some(on) = &j.on {
            exprs.push(on);
        }
    }
    if let Some(gs) = &s.group_by {
        for g in gs {
            exprs.push(g);
        }
    }
    if let Some(h) = &s.having {
        exprs.push(h);
    }
    for o in &s.order_by {
        exprs.push(&o.expr);
    }
    let mut correlated = false;
    for e in exprs {
        visit_expr_columns_and_subqueries(
            e,
            &mut |c| {
                if let Some(q) = &c.qualifier
                    && !inner.iter().any(|a| a.eq_ignore_ascii_case(q))
                {
                    correlated = true;
                }
            },
            &mut |_| {},
        );
    }
    correlated
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
        restrict: Option<(&[Row], &EvalContext<'_>)>,
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
            expr: Expr::Column(inner_col.clone()),
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
        // v7.32 (architecture v2 P3) — keyed index-probe. When the
        // caller hands a restriction set (the ≤LIMIT surviving outer
        // rows of a post-LIMIT deferred subquery) AND the correlation
        // column is backed by an index, evaluate only the surviving
        // correlation keys via per-key index seek instead of scanning
        // the whole inner relation. This is PG's SubPlan with an index
        // scan: 50 seeks of ~µs each vs a 24k-row all-keys batch
        // (~16 ms). The grouping below is shared — keyed result ≡
        // full-batch result for the covered keys, so semantics are
        // identical.
        //
        // The inner relation may itself be a join. The correlation
        // column names the *driving* table; PG, MySQL and MariaDB all
        // plan a correlated join subquery the same way — seek the
        // correlation index, then index-nested-loop to the joined
        // table. We promote that table to drive `batch` (an all-INNER
        // chain only) so the per-key `inner_col = <lit>` predicate
        // becomes a primary index seek and the existing INL path joins
        // the rest. A correlation column without a usable index, or a
        // join the promotion can't safely reorder, returns None and
        // the caller falls back to the lazy all-keys batch (no
        // regression).
        let keyed: Option<(&[Row], &EvalContext<'_>)> = restrict.and_then(|(rows, rctx)| {
            // Resolve the table that owns the correlation column.
            let driver_name: &str = if from.joins.is_empty() {
                from.primary.name.as_str()
            } else {
                let q = inner_col.qualifier.as_deref()?;
                let primary_alias = from
                    .primary
                    .alias
                    .as_deref()
                    .unwrap_or(from.primary.name.as_str());
                if primary_alias.eq_ignore_ascii_case(q) {
                    from.primary.name.as_str()
                } else {
                    from.joins
                        .iter()
                        .find(|j| {
                            j.table
                                .alias
                                .as_deref()
                                .unwrap_or(j.table.name.as_str())
                                .eq_ignore_ascii_case(q)
                        })
                        .map(|j| j.table.name.as_str())?
                }
            };
            let table = self.active_catalog().get(driver_name)?;
            let pos = table
                .schema()
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&inner_col.name))?;
            table.index_on(pos)?;
            // For a join inner, drive the seek from the correlation
            // table so `inner_col = <lit>` lands as a primary index
            // seek (else the source-order primary scans the full
            // relation and the join hash-builds the whole peer — the
            // 12 GB all-keys hog R30 hit at prod scale).
            if !from.joins.is_empty() {
                let driver_alias = inner_col.qualifier.as_deref()?;
                if !reorder::drive_from(&mut batch, driver_alias) {
                    return None;
                }
            }
            Some((rows, rctx))
        });
        let rows = if let Some((restrict_rows, rctx)) = keyed {
            let mut seen: alloc::collections::BTreeSet<String> =
                alloc::collections::BTreeSet::new();
            let mut all_rows: Vec<Row> = Vec::new();
            for srow in restrict_rows {
                cancel.check()?;
                let kv = eval::eval_expr(&Expr::Column(outer_col.clone()), srow, rctx)
                    .map_err(EngineError::Eval)?;
                if matches!(kv, Value::Null) {
                    continue;
                }
                if !seen.insert(aggregate::encode_key(core::slice::from_ref(&kv))) {
                    continue;
                }
                let key_eq = Expr::Binary {
                    lhs: alloc::boxed::Box::new(Expr::Column(inner_col.clone())),
                    op: BinOp::Eq,
                    rhs: alloc::boxed::Box::new(value_to_literal_expr(kv)?),
                };
                let mut probe = batch.clone();
                probe.where_ = Some(match probe.where_.take() {
                    Some(w) => Expr::Binary {
                        lhs: alloc::boxed::Box::new(w),
                        op: BinOp::And,
                        rhs: alloc::boxed::Box::new(key_eq),
                    },
                    None => key_eq,
                });
                if let QueryResult::Rows { rows, .. } = self.exec_select_cancel(&probe, cancel)? {
                    all_rows.extend(rows);
                }
            }
            all_rows
        } else {
            let r = self.exec_select_cancel(&batch, cancel)?;
            let QueryResult::Rows { rows, .. } = r else {
                return Ok(None);
            };
            rows
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
        Expr::InList { expr, list, .. } => {
            collect_scalar_subqueries(expr, out);
            for item in list {
                collect_scalar_subqueries(item, out);
            }
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
        Expr::InList { expr, list, .. } => {
            hollow_scalar_subqueries(expr);
            for item in list.iter_mut() {
                hollow_scalar_subqueries(item);
            }
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
        Expr::InList { expr, list, .. } => {
            if !splice_planned_subqueries(expr, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for item in list.iter_mut() {
                if !splice_planned_subqueries(item, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// v7.30.2 (mailrs round-25) — minimum element count before an
/// all-literal `IN` list gets a per-query membership set. Below
/// this the linear scan wins on build cost.
const INLIST_SET_THRESHOLD: usize = 64;

/// Cheap pre-check: is a set-eligible `IN` list reachable on the
/// AND spine of this expression? Anything else keeps the plain
/// `eval_expr` path untouched.
fn expr_may_use_in_set(e: &Expr) -> bool {
    match e {
        Expr::InList { list, .. } => list.len() >= INLIST_SET_THRESHOLD,
        Expr::Binary {
            lhs,
            op: BinOp::And,
            rhs,
        } => expr_may_use_in_set(lhs) || expr_may_use_in_set(rhs),
        _ => false,
    }
}

/// Analyse an `IN` list for set eligibility: every element a literal,
/// all of one family (integer or string, NULLs tracked separately).
pub(crate) fn build_in_list_set(list: &[Expr]) -> Option<memoize::InListSetEntry> {
    let mut has_null = false;
    let mut ints: alloc::collections::BTreeSet<i64> = alloc::collections::BTreeSet::new();
    let mut texts: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    for item in list {
        let Expr::Literal(lit) = item else {
            return None;
        };
        match lit {
            Literal::Null => has_null = true,
            Literal::Integer(i) => {
                ints.insert(*i);
            }
            Literal::String(s) => {
                texts.insert(s.clone());
            }
            _ => return None,
        }
        if !ints.is_empty() && !texts.is_empty() {
            return None;
        }
    }
    let set = if !ints.is_empty() {
        memoize::InListSet::Int(ints)
    } else if !texts.is_empty() {
        memoize::InListSet::Text(texts)
    } else {
        return None;
    };
    Some(memoize::InListSetEntry { set, has_null })
}

/// Subquery-free eval that serves large all-literal `IN` lists from
/// a per-query membership set (cached in the memo by node address).
/// Walks only the AND spine; every other node — and every needle
/// whose runtime family doesn't match the set — falls through to
/// `eval_expr`, so coercion and error semantics stay identical.
fn eval_with_in_sets(
    e: &Expr,
    row: &Row,
    ctx: &EvalContext<'_>,
    m: &mut memoize::MemoizeCache,
) -> Result<Value, EngineError> {
    match e {
        Expr::Binary {
            lhs,
            op: BinOp::And,
            rhs,
        } => {
            // Mirror eval_expr: both sides evaluate (no short
            // circuit), then SQL three-valued AND.
            let l = eval_with_in_sets(lhs, row, ctx, m)?;
            let r = eval_with_in_sets(rhs, row, ctx, m)?;
            eval::and_3vl(l, r).map_err(EngineError::Eval)
        }
        Expr::InList {
            expr: lhs,
            list,
            negated,
        } if list.len() >= INLIST_SET_THRESHOLD => {
            let key = core::ptr::from_ref::<Expr>(e) as usize;
            let Some(entry) = m
                .in_sets
                .entry(key)
                .or_insert_with(|| build_in_list_set(list))
            else {
                return eval::eval_expr(e, row, ctx).map_err(EngineError::Eval);
            };
            let needle = eval::eval_expr(lhs, row, ctx).map_err(EngineError::Eval)?;
            let contained = match (&needle, &entry.set) {
                // Non-empty list + NULL needle → NULL (negation of
                // NULL is still NULL).
                (Value::Null, _) => return Ok(Value::Null),
                (Value::SmallInt(n), memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
                (Value::Int(n), memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
                (Value::BigInt(n), memoize::InListSet::Int(s)) => s.contains(n),
                (Value::Text(t), memoize::InListSet::Text(s)) => s.contains(t.as_str()),
                // Cross-family needle (e.g. Float vs integer list):
                // keep apply_binary's coercion / error behaviour.
                _ => return eval::eval_expr(e, row, ctx).map_err(EngineError::Eval),
            };
            let inner = if contained {
                Value::Bool(true)
            } else if entry.has_null {
                Value::Null
            } else {
                Value::Bool(false)
            };
            Ok(match (negated, inner) {
                (true, Value::Bool(b)) => Value::Bool(!b),
                (_, v) => v,
            })
        }
        _ => eval::eval_expr(e, row, ctx).map_err(EngineError::Eval),
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
        Expr::InList { expr, list, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            for item in list {
                substitute_in_expr(item, row, ctx, outer_alias);
            }
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

pub(crate) fn select_has_window(stmt: &SelectStatement) -> bool {
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
        Expr::InList { expr, list, .. } => {
            expr_has_window(expr) || list.iter().any(expr_has_window)
        }
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
        Expr::InList { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
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
        Expr::InList { expr, list, .. } => {
            rewrite_column_in_expr(expr, old, new);
            for item in list {
                rewrite_column_in_expr(item, old, new);
            }
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
        Expr::InList { expr, list, .. } => {
            substitute_expr(expr, params)?;
            for item in list {
                substitute_expr(item, params)?;
            }
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
        Expr::InList { expr, list, .. } => {
            rewrite_expr_clock(expr, now);
            for item in list {
                rewrite_expr_clock(item, now);
            }
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
pub(crate) fn apply_offset_and_limit(rows: &mut Vec<Row>, offset: Option<u32>, limit: Option<u32>) {
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
            spg_storage::IndexKey::from_value(&value).is_some(),
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
