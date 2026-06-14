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
use expr_analysis::*;
use index_access::*;
pub(crate) use orderby::{
    apply_offset_and_limit, apply_offset_and_limit_tagged, build_order_keys, canonical_value_repr,
    expand_group_by_all, order_by_value_cmp, partial_sort_tagged, render_histogram_bounds,
    resolve_order_by_position, sort_by_keys, sort_values_for_histogram, value_cmp, value_to_f64,
};
pub(crate) use subquery::{
    build_in_list_set, collect_scalar_subqueries, expr_has_subquery, expr_tree_has_subquery,
};
pub use substitute::substitute_placeholders;
use substitute::*;
use system_catalog::*;
use window::*;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{ColumnDef, ColumnName, Expr, SelectItem, SelectStatement, Statement};
// v7.16.0 — re-export the parsed-statement AST so downstream
// crates (spg-embedded → spg-sqlx) don't need a direct dep on
// spg-sql for the prepare/bind handle.
pub use spg_sql::ast::Statement as ParsedStatement;
use spg_sql::parser::{self, ParseError};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, StorageError, Value, VecEncoding};

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

/// v6.2.0 — true for engine-managed catalog tables that the bare
/// `ANALYZE` (no target) should skip. v6.2.0 has no internal
/// tables yet (publications / subscriptions / users / statistics
/// all live as engine fields, not catalog tables), so this is a
/// reserved future-proofing hook — every existing user table is
/// analysed.
const fn is_internal_table_name(_name: &str) -> bool {
    false
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
mod tests;
