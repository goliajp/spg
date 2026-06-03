//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
pub mod describe;
pub mod eval;
pub mod json;
pub mod memoize;
pub mod plan_cache;
pub mod publications;
pub mod query_stats;
pub mod reorder;
pub mod selectivity;
pub mod statistics;
pub mod subscriptions;
pub mod users;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement,
    CreatePublicationStatement, CreateSubscriptionStatement, CreateTableStatement,
    CreateUserStatement, Expr, FrameBound, FrameKind, FromClause, IndexMethod, InsertStatement,
    JoinKind, Literal, OrderBy, SelectItem, SelectStatement, Statement, UnOp, UnionKind,
    VecEncoding as SqlVecEncoding, WindowFrame,
};
use spg_sql::parser::{self, ParseError};
use spg_storage::{
    Catalog, ColumnSchema, DataType, IndexKey, Row, StorageError, Table, TableSchema, Value,
    VecEncoding,
};

use crate::eval::{EvalContext, EvalError};

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy)]
pub struct CancelToken<'a> {
    flag: Option<&'a core::sync::atomic::AtomicBool>,
}

impl<'a> CancelToken<'a> {
    #[must_use]
    pub const fn none() -> Self {
        Self { flag: None }
    }

    #[must_use]
    pub const fn from_flag(f: &'a core::sync::atomic::AtomicBool) -> Self {
        Self { flag: Some(f) }
    }

    #[must_use]
    pub fn is_cancelled(self) -> bool {
        self.flag
            .is_some_and(|f| f.load(core::sync::atomic::Ordering::Relaxed))
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

fn build_envelope(
    catalog: &[u8],
    users: &[u8],
    pubs: &[u8],
    subs: &[u8],
    stats: &[u8],
) -> Vec<u8> {
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
}

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
        }
    }

    /// Construct an engine restored from a previously-snapshotted catalog
    /// (see `snapshot()`).
    pub fn restore(catalog: Catalog) -> Self {
        Self {
            catalog,
            tx_catalogs: BTreeMap::new(),
            current_tx: None,
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

    fn active_catalog(&self) -> &Catalog {
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
        let mut stmt = parser::parse_statement(sql)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
            // v6.2.3 — cost-based JOIN reorder (read path).
            reorder::reorder_joins(s, &self.catalog, &self.statistics);
        }
        let result = match stmt {
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
            Statement::ShowTables => Ok(self.exec_show_tables()),
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
        let mut stmt = parser::parse_statement(sql)?;
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
    pub fn describe_prepared(
        &self,
        stmt: &Statement,
    ) -> (Vec<u32>, Vec<ColumnSchema>) {
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
        mut stmt: Statement,
        params: &[Value],
    ) -> Result<QueryResult, EngineError> {
        substitute_placeholders(&mut stmt, params)?;
        self.execute_stmt_with_cancel(stmt, CancelToken::none())
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
        let result = match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Update(s) => self.exec_update_cancel(&s, cancel),
            Statement::Delete(s) => self.exec_delete_cancel(&s, cancel),
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
            Statement::Begin => self.exec_begin(),
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
            Statement::Savepoint(name) => self.exec_savepoint(name),
            Statement::RollbackToSavepoint(name) => self.exec_rollback_to_savepoint(&name),
            Statement::ReleaseSavepoint(name) => self.exec_release_savepoint(&name),
            Statement::ShowTables => Ok(self.exec_show_tables()),
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
                                segment_owners.entry(*segment_id).or_insert_with(|| tname.clone());
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
                let owner = segment_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
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
    pub const fn with_slow_query_log(
        mut self,
        threshold_us: u64,
        logger: SlowQueryLogger,
    ) -> Self {
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
                Some(Row::new(alloc::vec![
                    Value::Text(name),
                    Value::Text(ddl),
                ]))
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
                Row::new(alloc::vec![Value::Text(String::from(name)), Value::Text(ddl)])
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
        let row = Row::new(alloc::vec![
            Value::BigInt(verified),
            Value::BigInt(broken),
        ]);
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
            let non_null: Vec<String> = non_null_values
                .iter()
                .map(canonical_value_repr)
                .collect();
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
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()));
        // Walk every row, evaluate WHERE then SET expressions. We
        // gather (position, new_values) tuples first and apply them
        // afterwards so the WHERE/RHS evaluation reads the original
        // row state — matches PG semantics (UPDATE doesn't see its
        // own writes).
        let mut planned: Vec<(usize, Vec<Value>)> = Vec::new();
        for (i, row) in table.rows().iter().enumerate() {
            // v4.5: cooperative cancel checkpoint every 256 rows so
            // a runaway UPDATE without WHERE doesn't drag past the
            // server's query-timeout watchdog.
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut new_vals = row.values.clone();
            for (pos, expr) in &targets {
                let v = eval::eval_expr(expr, row, &ctx)?;
                new_vals[*pos] =
                    coerce_value(v, schema_cols[*pos].ty, &schema_cols[*pos].name, *pos)?;
            }
            planned.push((i, new_vals));
        }
        let affected = planned.len();
        for (pos, vals) in planned {
            table.update_row(pos, vals)?;
        }
        // v6.2.1 — auto-analyze modified-row tracking for UPDATE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Collects matching
    /// positions then delegates to `Table::delete_rows` (single index
    /// rebuild for the batch).
    fn exec_delete_cancel(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
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

        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()));
        let mut positions: Vec<usize> = Vec::new();
        for (i, row) in table.rows().iter().enumerate() {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            let keep = if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                !matches!(cond, Value::Bool(true))
            } else {
                false
            };
            if !keep {
                positions.push(i);
            }
        }
        let affected = table.delete_rows(&positions) + cold_shadow_count;
        // v6.2.1 — auto-analyze modified-row tracking for DELETE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
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
        if e.analyze {
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
        let table = self
            .active_catalog_mut()
            .get_mut(&s.name)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound { name: s.name.clone() })
            })?;
        match s.target {
            spg_sql::ast::AlterTableTarget::SetHotTierBytes(n) => {
                table.schema_mut().hot_tier_bytes = Some(n);
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
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
        let spg_sql::ast::AlterIndexTarget::Rebuild { encoding } = target;
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
        let table_name = stmt.table.clone();
        match stmt.method {
            IndexMethod::BTree => table.add_index(stmt.name, &stmt.column)?,
            IndexMethod::Hnsw => {
                table.add_nsw_index(stmt.name, &stmt.column, spg_storage::NSW_DEFAULT_M)?;
            }
            // v6.7.1 — BRIN. Pure metadata; no in-memory data.
            IndexMethod::Brin => table.add_brin_index(stmt.name, &stmt.column)?,
        }
        // v6.3.1 — adding an index can change the optimal plan for
        // any cached query that references this table.
        self.plan_cache.evict_referencing(&table_name);
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
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        let cols = stmt
            .columns
            .into_iter()
            .map(column_def_to_schema)
            .collect::<Result<Vec<_>, _>>()?;
        self.active_catalog_mut()
            .create_table(TableSchema::new(stmt.name, cols))?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_insert(&mut self, stmt: InsertStatement) -> Result<QueryResult, EngineError> {
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
        let mut affected = 0usize;
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
                        None => col.default.clone().unwrap_or(Value::Null),
                    };
                    if col.auto_increment && raw.is_null() {
                        let next = table.next_auto_value(i).ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                col.name
                            ))
                        })?;
                        raw = Value::BigInt(next);
                    }
                    out.push(coerce_value(raw, col.ty, &col.name, i)?);
                }
                out
            } else {
                // 1-1 mapping fast path: single Vec alloc, no raw_tuple.
                let mut out = Vec::with_capacity(schema_cols_len);
                for (i, (col, expr)) in column_meta.iter().zip(tuple).enumerate() {
                    let mut raw = literal_expr_to_value(expr)?;
                    if col.auto_increment && raw.is_null() {
                        let next = table.next_auto_value(i).ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                col.name
                            ))
                        })?;
                        raw = Value::BigInt(next);
                    }
                    out.push(coerce_value(raw, col.ty, &col.name, i)?);
                }
                out
            };
            table.insert(Row::new(values))?;
            affected += 1;
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
    fn exec_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
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
        apply_offset_and_limit(&mut rows, stmt.offset, stmt.limit);
        Ok(QueryResult::Rows { columns, rows })
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_lines)] // huge match — splitting fragments the planner
    fn exec_bare_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
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
            let ctx = EvalContext::new(&empty_schema, None);
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
            return self.exec_joined_select(stmt, from);
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
        let ctx = EvalContext::new(schema_cols, Some(alias));

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
        let indexed_rows: Option<Vec<Cow<'_, Row>>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek(w, schema_cols, self.active_catalog(), table, alias));

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
            let mut agg = aggregate::run(stmt, &filtered, schema_cols, Some(alias))?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset, stmt.limit);
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, schema_cols, alias)?;

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
                let cond = self.eval_expr_with_correlated(
                    where_expr,
                    row,
                    &ctx,
                    cancel,
                    Some(&mut memo),
                )?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            }
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ctx)?);
            }
            let order_keys = if stmt.order_by.is_empty() {
                Vec::new()
            } else {
                build_order_keys(&stmt.order_by, row, &ctx)?
            };
            tagged.push((order_keys, Row::new(values)));
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
            let keep = if stmt.distinct {
                None
            } else {
                stmt.limit
                    .map(|l| l as usize + stmt.offset.map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }

        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);

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
    fn exec_joined_select(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
    ) -> Result<QueryResult, EngineError> {
        // Resolve every table reference up front so we surface
        // TableNotFound before we start the cartesian work.
        let primary_table = self
            .active_catalog()
            .get(&from.primary.name)
            .ok_or_else(|| StorageError::TableNotFound {
                name: from.primary.name.clone(),
            })?;
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        let mut joined_tables: Vec<(&Table, String, JoinKind, Option<&Expr>)> = Vec::new();
        for j in &from.joins {
            let t = self.active_catalog().get(&j.table.name).ok_or_else(|| {
                StorageError::TableNotFound {
                    name: j.table.name.clone(),
                }
            })?;
            let a = j
                .table
                .alias
                .as_deref()
                .unwrap_or(j.table.name.as_str())
                .to_string();
            joined_tables.push((t, a, j.kind, j.on.as_ref()));
        }

        // Build the combined schema: composite "alias.col" names so the
        // qualified-column resolver can find anything by exact match.
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &primary_table.schema().columns {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{primary_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for (t, a, _, _) in &joined_tables {
            for col in &t.schema().columns {
                combined_schema.push(ColumnSchema::new(
                    alloc::format!("{a}.{}", col.name),
                    col.ty,
                    col.nullable,
                ));
            }
        }
        let ctx = EvalContext::new(&combined_schema, None);

        // Nested-loop join. Starting set: every primary row, padded with
        // (no joined columns yet).
        let mut working: Vec<Row> = primary_table.rows().iter().cloned().collect();
        let mut produced_len = primary_table.schema().columns.len();
        for (t, _, kind, on) in &joined_tables {
            let right_arity = t.schema().columns.len();
            let mut next: Vec<Row> = Vec::new();
            for left in &working {
                let mut left_matched = false;
                for right in t.rows() {
                    let mut combined_vals = left.values.clone();
                    combined_vals.extend(right.values.iter().cloned());
                    // Pad combined to the eventual full width so the
                    // partial schema still matches positions used by ON.
                    let combined = Row::new(combined_vals);
                    let keep = if let Some(on_expr) = on {
                        let cond = eval::eval_expr(on_expr, &combined, &ctx)?;
                        matches!(cond, Value::Bool(true))
                    } else {
                        // CROSS / comma-list: every pair survives.
                        true
                    };
                    if keep {
                        next.push(combined);
                        left_matched = true;
                    }
                }
                if !left_matched && matches!(kind, JoinKind::Left) {
                    // LEFT OUTER JOIN: emit the left row with NULLs on
                    // the right side when no peer matched.
                    let mut combined_vals = left.values.clone();
                    for _ in 0..right_arity {
                        combined_vals.push(Value::Null);
                    }
                    next.push(Row::new(combined_vals));
                }
            }
            working = next;
            produced_len += right_arity;
            debug_assert!(produced_len <= combined_schema.len());
        }

        // WHERE filter against combined rows.
        let mut filtered: Vec<Row> = Vec::new();
        for row in working {
            if let Some(where_expr) = &stmt.where_ {
                let cond = eval::eval_expr(where_expr, &row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            filtered.push(row);
        }

        // Aggregate path: handle GROUP BY / aggregate calls over the
        // joined+filtered rows.
        if aggregate::uses_aggregate(stmt) {
            let refs: Vec<&Row> = filtered.iter().collect();
            let mut agg = aggregate::run(stmt, &refs, &combined_schema, None)?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset, stmt.limit);
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, &combined_schema, "")?;
        let mut tagged: Vec<(Vec<f64>, Row)> = Vec::new();
        for row in &filtered {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ctx)?);
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
                stmt.limit
                    .map(|l| l as usize + stmt.offset.map_or(0, |o| o as usize))
            };
            let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
            partial_sort_tagged(&mut tagged, keep, &descs);
        }
        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);
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
    let limit = usize::try_from(stmt.limit?).ok()?;
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
    apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);
    let columns: Vec<ColumnSchema> = projection
        .into_iter()
        .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
        .collect();
    Ok(QueryResult::Rows {
        columns,
        rows: output_rows,
    })
}

fn try_index_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
) -> Option<Vec<Cow<'a, Row>>> {
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
        // Vector and Interval literals can't be used as B-tree index keys.
        // Tell the planner to fall back to full-scan.
        Literal::Vector(_) | Literal::Interval { .. } => return None,
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
                // nullability). Compound expressions evaluate fine but have
                // no static type — surface them as nullable TEXT, which is
                // what most clients render anyway.
                if let Expr::Column(c) = expr {
                    let sch = resolve_projection_column(c, schema_cols, table_alias)?;
                    let output_name = alias.clone().unwrap_or_else(|| c.name.clone());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: sch.ty,
                        nullable: sch.nullable,
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
        // For v4.12 we only support a single-table FROM. Joins +
        // windows is queued for v5.x.
        if !from.joins.is_empty() {
            return Err(EngineError::Unsupported(
                "JOIN with window functions not yet supported".into(),
            ));
        }
        let primary = &from.primary;
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let schema_cols = &table.schema().columns;
        let ctx = EvalContext::new(schema_cols, Some(alias));

        // 1) Filter pass.
        let mut filtered: Vec<&Row> = Vec::new();
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
            filtered.push(row);
        }
        let n_rows = filtered.len();

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
            let mut indexed: Vec<(Vec<Value>, Vec<(Value, bool)>, usize)> =
                Vec::with_capacity(n_rows);
            for (i, row) in filtered.iter().enumerate() {
                let pkey: Vec<Value> = partition_by
                    .iter()
                    .map(|p| eval::eval_expr(p, row, &ctx))
                    .collect::<Result<_, _>>()?;
                let okey: Vec<(Value, bool)> = order_by
                    .iter()
                    .map(|(e, desc)| eval::eval_expr(e, row, &ctx).map(|v| (v, *desc)))
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
                    &filtered,
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

        // 7) Project into final rows.
        let ext_ctx = EvalContext::new(&ext_cols, Some(alias));
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
        apply_offset_and_limit(&mut out_rows, stmt.offset, stmt.limit);
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
                let body_result = self.exec_select_cancel(&cte.body, cancel)?;
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
                for (e, _) in order_by {
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
        memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<Value, EngineError> {
        if !expr_has_subquery(expr) {
            return eval::eval_expr(expr, row, ctx).map_err(EngineError::Eval);
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
            Expr::ScalarSubquery(inner) => {
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
            Expr::WindowFunction { .. } | Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
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

fn expr_refers_to(e: &Expr, target: &str) -> bool {
    match e {
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
                || order_by.iter().any(|(o, _)| expr_refers_to(o, target))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
    }
}

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
fn substitute_outer_columns(stmt: &mut SelectStatement, row: &Row, ctx: &EvalContext<'_>) {
    let Some(outer_alias) = ctx.table_alias else {
        return;
    };
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
    if let Expr::Column(c) = e
        && let Some(qual) = &c.qualifier
        && qual.eq_ignore_ascii_case(outer_alias)
    {
        // Look up the column's index in the outer schema.
        if let Some(idx) = ctx
            .columns
            .iter()
            .position(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        {
            let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
            if let Ok(lit) = value_to_literal_expr(v) {
                *e = lit;
                return;
            }
        }
    }
    match e {
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
            for (o, _) in order_by {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
        }
        Expr::ScalarSubquery(s) => substitute_in_select(s, row, ctx, outer_alias),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            substitute_in_select(subquery, row, ctx, outer_alias);
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
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

fn order_key_cmp(a: &[(Value, bool)], b: &[(Value, bool)]) -> core::cmp::Ordering {
    for ((va, desc), (vb, _)) in a.iter().zip(b.iter()) {
        let c = value_cmp(va, vb);
        let c = if *desc { c.reverse() } else { c };
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

#[allow(clippy::match_same_arms)] // explicit arms per type document the supported pairs
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
    slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)],
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
            let mut prev_key: Option<&[(Value, bool)]> = None;
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
            let mut prev_key: Option<&[(Value, bool)]> = None;
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
                    if target_signed < 0
                        || target_signed >= i64::try_from(n).unwrap_or(i64::MAX)
                    {
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
            let mut prev_key: Option<&[(Value, bool)]> = None;
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
    slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)],
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
fn peer_group_start(slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)], i: usize) -> usize {
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
fn peer_group_end(slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)], i: usize) -> usize {
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

fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
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
                || order_by.iter().any(|(e, _)| expr_has_subquery(e))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
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
fn substitute_placeholders(stmt: &mut Statement, params: &[Value]) -> Result<(), EngineError> {
    match stmt {
        Statement::Select(s) => substitute_select(s, params)?,
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    substitute_expr(e, params)?;
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

fn substitute_select(
    s: &mut SelectStatement,
    params: &[Value],
) -> Result<(), EngineError> {
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_expr(expr, params)?;
        }
    }
    if let Some(w) = &mut s.where_ {
        substitute_expr(w, params)?;
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            substitute_expr(g, params)?;
        }
    }
    if let Some(h) = &mut s.having {
        substitute_expr(h, params)?;
    }
    for o in &mut s.order_by {
        substitute_expr(&mut o.expr, params)?;
    }
    for (_, peer) in &mut s.unions {
        substitute_select(peer, params)?;
    }
    Ok(())
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
            for (e, _) in order_by {
                substitute_expr(e, params)?;
            }
        }
        Expr::Literal(_) | Expr::Column(_) => {}
        // Already handled above.
        Expr::Placeholder(_) => unreachable!("Placeholder handled at top of fn"),
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
        Value::Interval { months, micros } => eval::format_interval(*months, *micros),
        Value::Numeric { scaled, scale } => eval::format_numeric(*scaled, *scale),
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            // Unreachable in practice (vector columns are filtered
            // out before this). Defensive fallback so a future
            // vector-stats path doesn't crash.
            alloc::format!("{v:?}")
        }
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
        Value::Numeric { scaled, scale } => {
            Literal::String(eval::format_numeric(scaled, scale))
        }
        Value::Date(d) => Literal::String(eval::format_date(d)),
        Value::Timestamp(t) => Literal::String(eval::format_timestamp(t)),
        Value::Interval { months, micros } => Literal::Interval {
            months,
            micros,
            text: eval::format_interval(months, micros),
        },
        // SQ8 / halfvec cells dequantise to f32 before reaching the
        // substitute walker; pgwire's Bind path handles that.
        Value::Sq8Vector(q) => Literal::Vector(spg_storage::quantize::dequantize(&q)),
        Value::HalfVector(h) => Literal::Vector(h.to_f32_vec()),
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
        }
        _ => {}
    }
}

fn rewrite_select_clock(s: &mut SelectStatement, now: i64) {
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            rewrite_expr_clock(expr, now);
        }
    }
    if let Some(w) = &mut s.where_ {
        rewrite_expr_clock(w, now);
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            rewrite_expr_clock(g, now);
        }
    }
    if let Some(h) = &mut s.having {
        rewrite_expr_clock(h, now);
    }
    for o in &mut s.order_by {
        rewrite_expr_clock(&mut o.expr, now);
    }
    for (_, peer) in &mut s.unions {
        rewrite_select_clock(peer, now);
    }
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
            for (e, _) in order_by {
                rewrite_expr_clock(e, now);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
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
    // ASCII case-insensitive name match. Limited to the three keywords
    // that actually need rewriting.
    let matched = match name.len() {
        3 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("now") => Some(true),
        12 if name.eq_ignore_ascii_case("current_date") => Some(false),
        17 if name.eq_ignore_ascii_case("current_timestamp") => Some(true),
        _ => None,
    };
    let is_timestamp = matched?;
    let payload = if is_timestamp {
        now
    } else {
        now.div_euclid(86_400_000_000)
    };
    let target = if is_timestamp {
        spg_sql::ast::CastTarget::Timestamp
    } else {
        spg_sql::ast::CastTarget::Date
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
fn partial_sort_tagged(
    tagged: &mut Vec<(Vec<f64>, Row)>,
    keep: Option<usize>,
    descs: &[bool],
) {
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
        keys.push(value_to_order_key(&v)?);
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

fn column_def_to_schema(c: ColumnDef) -> Result<ColumnSchema, EngineError> {
    let ty = column_type_to_data_type(c.ty);
    let mut schema = ColumnSchema::new(c.name.clone(), ty, c.nullable);
    if let Some(default_expr) = c.default {
        // DEFAULT must be a literal expression — evaluated at CREATE TABLE
        // time against an empty row context. Any column ref / aggregate
        // surfaces as the corresponding eval error.
        let raw = literal_expr_to_value(default_expr)?;
        let coerced = coerce_value(raw, ty, &c.name, 0)?;
        schema = schema.with_default(coerced);
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
        ColumnTypeName::Json => DataType::Json,
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
        other => Err(EngineError::Unsupported(alloc::format!(
            "non-literal INSERT value expression: {other:?}"
        ))),
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
    let coerced =
        match (v, expected) {
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
            (Value::SmallInt(n), DataType::Numeric { precision, scale }) => Some(
                numeric_from_integer(i128::from(n), precision, scale, col_name)?,
            ),
            (Value::BigInt(n), DataType::Int) => i32::try_from(n).ok().map(Value::Int),
            (Value::BigInt(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
            #[allow(clippy::cast_precision_loss)]
            (Value::BigInt(n), DataType::Float) => Some(Value::Float(n as f64)),
            (Value::BigInt(n), DataType::Numeric { precision, scale }) => Some(
                numeric_from_integer(i128::from(n), precision, scale, col_name)?,
            ),
            (Value::Float(x), DataType::Numeric { precision, scale }) => {
                Some(numeric_from_float(x, precision, scale, col_name)?)
            }
            // Text → DATE / TIMESTAMP: parse canonical text forms.
            (Value::Text(s), DataType::Date) => {
                let d = eval::parse_date_literal(&s).ok_or_else(|| {
                    EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot parse {s:?} as DATE for column `{col_name}`"
                        ),
                    })
                })?;
                Some(Value::Date(d))
            }
            // v4.9: Text ↔ JSON coercion. No structural validation —
            // any text literal is accepted; the responsibility for
            // valid JSON lies with the producer.
            (Value::Text(s), DataType::Json) => Some(Value::Json(s)),
            (Value::Json(s), DataType::Text) => Some(Value::Text(s)),
            (Value::Text(s), DataType::Timestamp) => {
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
            (Value::Date(d), DataType::Timestamp) => {
                Some(Value::Timestamp(i64::from(d) * 86_400_000_000))
            }
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
            ) if v.len() == dim as usize => {
                Some(Value::Sq8Vector(spg_storage::quantize::quantize(&v)))
            }
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
            e.execute_prepared(
                stmt.clone(),
                &[Value::Int(id), Value::Text(name.into())],
            )
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
        let stmt = e
            .prepare("SELECT id FROM t WHERE v = $1")
            .unwrap();
        let QueryResult::Rows { rows, .. } = e
            .execute_prepared(stmt, &[Value::Int(35)])
            .unwrap()
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
    fn insert_non_literal_expr_unsupported() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        let err = e.execute("INSERT INTO foo VALUES (1 + 2)").unwrap_err();
        assert!(matches!(err, EngineError::Unsupported(_)));
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
        e.execute("CREATE PUBLICATION pub_b FOR ALL TABLES").unwrap();
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
        assert_eq!(s1.publications, alloc::vec!["p1".to_string(), "p2".to_string()]);
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

    #[test]
    // ── v6.2.0: ANALYZE + spg_statistic + envelope v5 ──────────

    #[test]
    fn analyze_populates_histogram_bounds() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
        for i in 0..50 {
            e.execute(&alloc::format!(
                "INSERT INTO t VALUES ({i}, 'name{i}')"
            ))
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
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        let n1 = e.statistics().get("t", "id").unwrap().n_distinct;
        assert_eq!(n1, 10);
        for i in 10..30 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        let n2 = e.statistics().get("t", "id").unwrap().n_distinct;
        assert_eq!(n2, 30);
    }

    #[test]
    fn analyze_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("ANALYZE nonexistent").unwrap_err();
        assert!(matches!(err, EngineError::Storage(StorageError::TableNotFound { .. })));
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
            QueryResult::CommandOk { affected, modified_catalog } => {
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
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
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
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
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
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        assert!(e.tables_needing_analyze().is_empty(), "fresh ANALYZE");
        for i in 1000..1050 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        assert!(
            e.tables_needing_analyze().is_empty(),
            "50 inserts < threshold of ~105"
        );
        for i in 1050..1200 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
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
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i})")).unwrap();
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
        e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT)").unwrap();
        for i in 0..50 {
            e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'x')"))
                .unwrap();
        }
        e.execute("ANALYZE t").unwrap();
        // UPDATE 20 rows + DELETE 5 → modified=25. Threshold = 0.1
        // × max(50, 100) = 10. So 25 >= 10 → trigger.
        e.execute("UPDATE t SET label = 'y' WHERE id < 20").unwrap();
        e.execute("DELETE FROM t WHERE id >= 45").unwrap();
        assert_eq!(
            e.tables_needing_analyze(),
            alloc::vec!["t".to_string()]
        );
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
        e.create_user(
            "alice",
            "secret",
            crate::users::Role::ReadOnly,
            [0u8; 16],
        )
        .unwrap();

        // Forge an envelope v2: same shape as v3 but no pubs trailer.
        let catalog = e.catalog.serialize();
        let users = crate::users::serialize_users(&e.users);
        let mut buf = Vec::new();
        buf.extend_from_slice(b"SPGENV01");
        buf.push(2u8); // v2
        buf.extend_from_slice(
            &u32::try_from(catalog.len()).unwrap().to_le_bytes(),
        );
        buf.extend_from_slice(&catalog);
        buf.extend_from_slice(
            &u32::try_from(users.len()).unwrap().to_le_bytes(),
        );
        buf.extend_from_slice(&users);
        let crc = spg_crypto::crc32::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let e2 = Engine::restore_envelope(&buf).expect("v2 envelope restores");
        assert!(e2.publications().is_empty());
    }
}
