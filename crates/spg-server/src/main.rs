//! SPG daemon — TCP listener that accepts wire frames and dispatches them.
//!
//! v0.10 CLI:
//!
//! ```text
//! spg-server [addr] [db_path] [audit_path] [wal_path]
//! ```
//!
//! - `addr` defaults to `127.0.0.1:5544`.
//! - `db_path` (2nd positional) is the catalog snapshot file. With WAL off it
//!   is rewritten atomically after every successful DDL / DML. With WAL on
//!   it's only the *initial* checkpoint — runtime writes go to the WAL.
//! - `audit_path` enables an append-only BLAKE3 hash-chain audit log — every
//!   successful DDL / DML is bound to the previous entry by hash, so any
//!   tamper / reorder / splice is caught on startup.
//! - `wal_path` enables a write-ahead log: every successful `CommandOk` SQL
//!   text is appended (length-prefixed, fsync'd). On startup the WAL is
//!   replayed on top of the db snapshot; an open transaction at end-of-WAL
//!   (e.g. server crash mid-COMMIT) is auto-rolled-back.
//!
//! Pass `-` (or omit) to skip any positional after the first.

mod alloc_budget;
mod backup;
mod flusher;
mod freezer;
mod manifest;
mod observability;
mod pgwire;
mod replication;
mod scram;

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_audit::AuditLog;
use spg_engine::{Engine, EngineError, QueryResult, Role};
use spg_storage::{ColumnSchema, DataType, Row, Value};
use spg_wire::{
    ColumnDesc, Frame, FrameError, Op, WireType, WireValue, build_command_complete, build_data_row,
    build_data_row_batch, build_error_response, build_row_description, build_stats_response,
    decode, encode, parse_auth, parse_auth_user, parse_query,
};

/// v5.5.1: custom global allocator that enforces the per-query memory budget
/// (`SPG_MAX_QUERY_BYTES`). See the `alloc_budget` module for the model.
#[global_allocator]
static GLOBAL_ALLOC: alloc_budget::BudgetAllocator = alloc_budget::BudgetAllocator;

/// v5.5.1: default `SPG_MAX_QUERY_BYTES` when the env is unset — 256 MiB.
/// A runaway-query safety net that is on by default; set `SPG_MAX_QUERY_BYTES=0`
/// to disable (unlimited).
const DEFAULT_MAX_QUERY_BYTES: u64 = 256 * 1024 * 1024;

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_CHUNK: usize = 4096;
/// Rows per `DataRowBatch` frame (v3.3.0). Caps in-memory frame size
/// on huge SELECTs while still amortising the per-frame header.
const BATCH_ROWS_PER_FRAME: usize = 256;
/// v4.33: cadence at which the accept loop polls the shutdown flag
/// and the drain loop polls `active_connections`. 50 ms keeps SIGTERM
/// → process-exit latency under ~100 ms when no in-flight work
/// remains, without burning a measurable CPU slice when idle.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);
/// v4.33: default `SPG_SHUTDOWN_DEADLINE_SEC`. Mirrors systemd's
/// `DefaultTimeoutStopSec` so operators don't surprise the supervisor.
const DEFAULT_SHUTDOWN_DEADLINE_SEC: u64 = 30;
/// v4.42: cap on tasks the commit-barrier leader pulls into one
/// group before fsyncing. A single group is one batched
/// `write_all` + one `sync_data` regardless of group size, so the
/// bigger the group the better the fsync amortisation — but the
/// per-group prepare (sequential `execute_in(sql, t)` under the
/// engine write lock) is linear in group size, so an unbounded
/// group would let a single bursty client starve readers behind
/// `engine.write()`. 16 is the same heuristic PG's `commit_delay`
/// uses as a sensible upper bound for default workloads; can be
/// raised via `SPG_COMMIT_GROUP_MAX` for benchmark experiments
/// where prepare-time is small (single-row INSERTs).
const DEFAULT_COMMIT_GROUP_MAX: usize = 16;

/// v4.33 graceful shutdown — flipped by the SIGTERM/SIGINT handler.
/// The main accept loop polls this between non-blocking accepts so it
/// can break out, then waits for active connections to drain (bounded
/// by `SPG_SHUTDOWN_DEADLINE_SEC`) before returning. Async-signal-safe:
/// the handler only does an atomic store.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// v4.2 + v4.5 resource limits. Each field is `None` = unlimited.
#[derive(Debug, Default, Clone, Copy)]
struct Limits {
    /// Maximum concurrent client connections. New accepts beyond
    /// this number get a clear error and the socket closes
    /// immediately. None = unlimited.
    max_connections: Option<usize>,
    /// Maximum rows a single SELECT may return. Enforced inside the
    /// engine so a runaway full-scan can't blow the server's heap
    /// before the result is shaped into wire frames.
    max_query_rows: Option<usize>,
    /// v5.5.1: per-query memory ceiling in bytes, enforced at the global
    /// allocator (see `alloc_budget`). `None` = use `DEFAULT_MAX_QUERY_BYTES`
    /// (256 MiB, on by default); explicit `0` via `SPG_MAX_QUERY_BYTES=0` =
    /// unlimited. A query whose live allocation crosses the ceiling trips its
    /// cancel flag and bails with `EngineError::Cancelled`.
    max_query_bytes: Option<u64>,
    /// v4.5: per-query wall-clock budget (milliseconds). When set, a
    /// watchdog thread starts on each `Query` frame, flips a
    /// `CancelToken` after the budget, and shuts down the TCP stream
    /// so a stuck server thread can't hold the connection open
    /// past the budget either.
    query_timeout_ms: Option<u64>,
    /// v4.5: close a connection that has been idle (no incoming
    /// frame) for this many seconds. Implemented via the OS
    /// read timeout on the TCP socket — when `read()` returns
    /// `WouldBlock` the handle loop exits cleanly.
    idle_timeout_sec: Option<u64>,
    /// v4.33: when set, every query whose dispatch wall-clock
    /// exceeds this threshold (milliseconds) emits one JSON line
    /// on stderr. Mirrors `SPG_LOG_FORMAT=json`'s field naming so
    /// the same log pipeline can ingest both. Defaults off.
    slow_query_log_ms: Option<u64>,
    /// v4.33: when set, the WAL appender refuses writes whose
    /// volume's free space (per `statvfs`) is below this byte
    /// count. Returns `ErrorKind::StorageFull` with a clear
    /// message; reads keep serving. Defaults off.
    wal_min_free_bytes: Option<u64>,
    /// v4.33: bound the time SIGTERM/SIGINT waits for active
    /// connections to drain before `process::exit(0)`. None means
    /// "use `DEFAULT_SHUTDOWN_DEADLINE_SEC`" — there is no
    /// "wait forever" mode (operators wanting that don't need a
    /// signal handler at all).
    shutdown_deadline_sec: Option<u64>,
}

/// v4.29 chaos: when set, the WAL appender refuses any write that
/// would push the on-disk WAL past this byte count. Returns a
/// clear ENOSPC-like error to the caller; the server stays alive
/// and previously committed state is intact. Zero cost when unset.
#[derive(Debug, Default, Clone, Copy)]
struct ChaosKnobs {
    wal_quota_bytes: Option<u64>,
    /// v4.34: when true, the dispatch-time preflight check that
    /// rejects oversize writes before any engine mutation is
    /// skipped. The append still fails inside `append_wal*`, which
    /// is exactly what exercises the implicit-BEGIN..COMMIT
    /// rollback path end-to-end (chaos test asserts no phantom
    /// row survives). Test-only — production deployments leave
    /// this false.
    disable_wal_preflight: bool,
}

/// v4.42 — outcome a `CommitTask` is acked with by the group leader.
/// `result` is the engine-level outcome from the auto-commit wrap
/// (prepare → `execute_in(sql)` → install via `execute_in("COMMIT", ...)`);
/// `wal_outcome` is the leader's batched `write_all` + `sync_data`
/// result, so each follower can keep the v4.41.1 "WAL append failed:
/// ..." error shape on the wire. On `wal_outcome` Err the leader has
/// already issued `execute_in("ROLLBACK", t)` for every survivor —
/// the dispatch thread reads `wal_outcome` only to surface the error
/// to the client.
struct CommitResult {
    result: Result<QueryResult, EngineError>,
    wal_outcome: std::io::Result<()>,
}

/// v4.42 — one entry in the commit-barrier queue. The dispatch thread
/// pushes `{ sql, cancel_flag, ack }` and waits on its `ack` channel;
/// the elected leader drains the queue under `engine.write()`, runs
/// each task's BEGIN+sql in its own `TxId` slot under that task's
/// `cancel_flag` (so per-query watchdog timeouts still fire even
/// when the SQL is being executed by another connection's leader
/// loop), batches the WAL bytes, fsyncs once, then installs
/// (COMMIT or ROLLBACK per `wal_outcome`) and fans out the
/// `CommitResult` to every task's `ack`.
struct CommitTask {
    sql: String,
    cancel_flag: Arc<AtomicBool>,
    ack: SyncSender<CommitResult>,
}

/// v4.42 — shared commit-barrier state. The mutex serialises queue
/// pushes against leader drains; `leader_active` is the latch that
/// decides whether an arriving task drives the group itself (it
/// becomes the leader) or just waits on its `ack` channel for a
/// concurrent leader to ack it. Rolling drain: the leader loops
/// until it observes `pending.is_empty()` under this mutex, then
/// flips `leader_active` back to false and exits.
struct CommitQueueState {
    pending: VecDeque<CommitTask>,
    leader_active: bool,
}

pub(crate) struct ServerState {
    /// v4.0: `RwLock` instead of `Mutex` so read-only statements
    /// (SELECT / SHOW outside an active TX) can run in parallel
    /// across connections. The write path takes `.write()`; the
    /// read path takes `.read()` and uses `Engine::execute_readonly`.
    pub(crate) engine: RwLock<Engine>,
    db_path: Option<PathBuf>,
    audit_log: Mutex<AuditLog>,
    audit_path: Option<PathBuf>,
    wal: Option<Mutex<File>>,
    /// v5.4.4 — `try_clone`'d handle to the same underlying WAL
    /// file. The kernel sees both handles as referring to the
    /// same file, so `sync_data` on either flushes the file's
    /// data; **but** `sync_data` does not require exclusive
    /// access, only `&File`. The async-commit flusher uses this
    /// handle to fsync **without holding `wal` mutex** — under a
    /// 5 ms APFS fsync latency a per-write client takes the
    /// mutex for microseconds (`write_all` only) and runs at
    /// non-fsync-bound throughput. Stays `None` when no WAL is
    /// configured.
    wal_sync_clone: Option<Arc<File>>,
    /// Kept so the path can be reported in error messages; runtime appends go
    /// through `wal` directly.
    wal_path: Option<PathBuf>,
    /// v4.42: commit-barrier queue used by the auto-commit wrap path
    /// (`Op::Query`, `needs_wrap` branch). Dispatch threads push a
    /// `CommitTask` onto `pending` and wait on its `ack` channel;
    /// the first arrival flips `leader_active = true` and drives
    /// the group through `run_leader_commit_round`. Inert when
    /// `wal` is `None` (the wrap path doesn't exist without WAL).
    commit_queue: Mutex<CommitQueueState>,
    /// Optional single password — Redis/Valkey style. `Some` means the
    /// server demands `AUTH <password>` before any non-Ping frame; `None`
    /// means open access (the default).
    password: Option<String>,
    /// v4.2: configured resource limits.
    limits: Limits,
    /// v4.2: live connection counter, used to enforce
    /// `limits.max_connections`. Incremented at accept, decremented
    /// when the handle thread's `ConnectionGuard` drops.
    active_connections: AtomicUsize,
    /// v4.13: observability counters surfaced via /metrics. Cheap
    /// Relaxed atomics — increment from any dispatch site.
    metrics: Arc<observability::Metrics>,
    /// v6.1.4 logical-replication subscriber-worker registry. Each
    /// active subscription has a row mapping its name to a shutdown
    /// flag the worker polls between frame reads. `reconcile_
    /// subscriptions` adds rows when CREATE SUBSCRIPTION runs and
    /// flips the flag when DROP SUBSCRIPTION runs.
    sub_workers: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    /// v6.1.6 — stable per-cluster identifier used by MAGIC_SUB
    /// cycle detection. Loaded from the `<wal_path>.cluster_id`
    /// sidecar at startup (or `<db_path>.cluster_id` if no wal);
    /// generated randomly on first boot when no sidecar exists.
    /// Servers with neither path get a random `cluster_id` that
    /// only lives for the process lifetime — fine for tests but
    /// not for production replicas. The master sends its
    /// cluster_id in the MAGIC_SUB handshake reply; the subscriber
    /// aborts the link when it equals its own (direct
    /// self-subscription detection). v6.1.6 ships direct-cycle
    /// detection only; indirect cycles (A → B → A through a
    /// chain) need WAL-record-level originator tagging — out of
    /// v6.1 scope.
    pub(crate) cluster_id: u64,
    /// v6.1.8 — `effective_wal_level`. `0` = replica (default,
    /// legacy MAGIC_V1 / MAGIC_V2 followers only); `1` = logical
    /// (MAGIC_SUB subscriptions accepted in addition). Flipped
    /// at runtime via `SET effective_wal_level = 'logical'` /
    /// `SET effective_wal_level = 'replica'` and observable via
    /// `SHOW effective_wal_level`. Initial value comes from the
    /// `SPG_WAL_LEVEL` env var (`logical` / `replica`); defaults
    /// to `replica` so a fresh cluster doesn't expose the
    /// MAGIC_SUB surface until an operator opts in.
    pub(crate) wal_level: AtomicU8,
    /// v4.29: optional failure-injection knobs used by chaos tests.
    /// All branches default-off and skip the check entirely when
    /// the env var wasn't set on startup.
    chaos: ChaosKnobs,
    /// v4.36: follower-side replication lag tracking. Populated by
    /// `replication::run_follower` when this node negotiated the v2
    /// protocol (status frames). On a primary or a v1-only follower
    /// these stay at zero and `/metrics` omits the series.
    pub(crate) lag_state: Arc<replication::LagState>,
    /// v5.1: cold-tier segments queued for lazy preload. Each spec
    /// is parsed from `SPG_PRELOAD_COLD_SEGMENT` at startup; the
    /// first dispatched `Op::Query` checks each unloaded spec for
    /// `(table, index)` existence and, when both are present, reads
    /// the segment file, registers it via `Catalog::load_segment_
    /// bytes`, and wires every PK in the segment as a Cold locator
    /// on the named index. `all_done` short-circuits the per-query
    /// check once every spec has been loaded.
    cold_preload: Vec<ColdPreloadSpec>,
    cold_preload_done: AtomicBool,
    /// v5.2.1: hot-tier byte budget in bytes. Parsed from
    /// `SPG_HOT_TIER_BYTES` at startup; defaults to 4 GiB
    /// (`DEFAULT_HOT_TIER_BYTES`). v5.2.1 ships as measurement only —
    /// the value is exposed via `/metrics` (`spg_hot_tier_bytes_budget`)
    /// alongside the live `Catalog::hot_tier_bytes()` reading
    /// (`spg_hot_tier_bytes_used`) so operators can chart how close
    /// the workload runs to the budget. v5.2.2 wires this as the
    /// freezer wake-up threshold.
    pub(crate) hot_tier_byte_budget: u64,
    /// v5.3.1: paths every cold-tier segment was loaded or written
    /// from. Maps `segment_id` (the value
    /// `Catalog::load_segment_bytes` returned at load time) →
    /// absolute path on disk. The freezer's `persist_segment` and
    /// the `try_lazy_preload_cold` env-var path both populate it;
    /// the manifest writer reads it to build the cold-tier
    /// registry. Held behind a `Mutex` because freezer + dispatch
    /// + snapshot-write can hit it concurrently.
    pub(crate) cold_segment_paths: Mutex<BTreeMap<u32, PathBuf>>,
    /// v6.5.2 — per-pgwire-connection state registry. Each accepted
    /// connection registers a `ConnState` here; the connection
    /// thread updates `current_sql` / `wait_event` / `elapsed_us` /
    /// `in_transaction` during its lifetime, and deregisters on
    /// close. Surfaced through `spg_stat_activity` virtual table via
    /// the engine's registered activity provider.
    pub(crate) connections: RwLock<Vec<Arc<ConnState>>>,
}

/// v6.5.2 — one row of `spg_stat_activity`'s per-connection state.
/// Lives behind `Arc` so the connection thread keeps one handle and
/// the registry keeps another; both can update the inner atomics
/// without locking.
pub(crate) struct ConnState {
    pub(crate) pid: u32,
    pub(crate) user: String,
    pub(crate) started_at_us: i64,
    pub(crate) current_sql: RwLock<String>,
    /// 0 = idle, 1 = write_lock, 2 = fsync, 3 = group_commit.
    /// String mapping handled at snapshot time.
    pub(crate) wait_event: AtomicU8,
    pub(crate) last_query_start_us: AtomicI64,
    pub(crate) in_transaction: AtomicBool,
}

impl ConnState {
    pub(crate) fn elapsed_us(&self) -> i64 {
        let start = self.last_query_start_us.load(Ordering::Relaxed);
        if start == 0 {
            return 0;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map_or(0, |d| d.as_micros() as i64);
        (now - start).max(0)
    }

    pub(crate) fn wait_event_str(&self) -> &'static str {
        match self.wait_event.load(Ordering::Relaxed) {
            1 => "write_lock",
            2 => "fsync",
            3 => "group_commit",
            _ => "",
        }
    }
}

/// v6.5.2 — global handle to `ServerState` so the engine's
/// `activity_provider` callback (a bare fn pointer that can't
/// capture state) can read from the live registry. Set once at
/// startup before any connection is accepted; read on every
/// `SELECT * FROM spg_stat_activity`.
pub(crate) static ACTIVITY_STATE: std::sync::OnceLock<Arc<ServerState>> =
    std::sync::OnceLock::new();

/// v6.5.3 — Engine-registered audit-chain provider. Snapshots
/// every entry in the live AuditLog as an `AuditRow`.
pub(crate) fn audit_chain_snapshot() -> Vec<spg_engine::AuditRow> {
    let Some(state) = ACTIVITY_STATE.get() else {
        return Vec::new();
    };
    let Ok(log) = state.audit_log.lock() else {
        return Vec::new();
    };
    log.entries()
        .iter()
        .map(|e| spg_engine::AuditRow {
            seq: i64::try_from(e.seq).unwrap_or(i64::MAX),
            ts_ms: i64::try_from(e.ts_ms).unwrap_or(i64::MAX),
            prev_hash_hex: hex_encode(&e.prev_hash),
            entry_hash_hex: hex_encode(&e.hash),
            sql: e.sql.clone(),
        })
        .collect()
}

/// v6.5.3 — Engine-registered chain verifier. Returns
/// `(verified_count, broken_at_seq)` — broken_at_seq is `-1` on
/// a clean chain (or empty log).
pub(crate) fn audit_verify_snapshot() -> (i64, i64) {
    let Some(state) = ACTIVITY_STATE.get() else {
        return (0, -1);
    };
    let Ok(log) = state.audit_log.lock() else {
        return (0, -1);
    };
    let n = log.entries().len() as i64;
    match log.verify() {
        Ok(()) => (n, -1),
        Err(spg_audit::AuditError::BrokenChain { seq })
        | Err(spg_audit::AuditError::HashMismatch { seq })
        | Err(spg_audit::AuditError::InvalidUtf8 { seq }) => {
            (i64::try_from(seq).unwrap_or(i64::MAX), i64::try_from(seq).unwrap_or(i64::MAX))
        }
        Err(_) => (0, 0),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// v6.5.2 — Engine-registered activity provider. Snapshots the
/// live `connections` registry into the `ActivityRow` shape the
/// engine renders.
pub(crate) fn activity_snapshot() -> Vec<spg_engine::ActivityRow> {
    let Some(state) = ACTIVITY_STATE.get() else {
        return Vec::new();
    };
    let Ok(conns) = state.connections.read() else {
        return Vec::new();
    };
    conns
        .iter()
        .map(|c| {
            let current_sql = c
                .current_sql
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();
            spg_engine::ActivityRow {
                pid: c.pid,
                user: c.user.clone(),
                started_at_us: c.started_at_us,
                current_sql,
                wait_event: c.wait_event_str().to_string(),
                elapsed_us: c.elapsed_us(),
                in_transaction: c.in_transaction.load(Ordering::Relaxed),
            }
        })
        .collect()
}

/// Default `SPG_HOT_TIER_BYTES` when the env var is unset / invalid —
/// 4 GiB, matching the `V5_DESIGN.md` L2 row for v5.2.
pub(crate) const DEFAULT_HOT_TIER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// v5.1: one entry in the `SPG_PRELOAD_COLD_SEGMENT` queue —
/// `table:index:path`. Loaded once and never reloaded; `loaded`
/// goes from false → true atomically.
struct ColdPreloadSpec {
    table: String,
    index: String,
    path: PathBuf,
    loaded: AtomicBool,
}

fn parse_optional_path(arg: Option<String>) -> Option<PathBuf> {
    arg.filter(|s| !s.is_empty() && s != "-").map(PathBuf::from)
}

/// Resolve a path setting from (CLI arg | env var) — CLI wins, env fills in
/// when the CLI slot is omitted (or passed as `-`).
fn resolve_path(cli: Option<String>, env_key: &str) -> Option<PathBuf> {
    parse_optional_path(cli).or_else(|| {
        env::var(env_key)
            .ok()
            .and_then(|s| parse_optional_path(Some(s)))
    })
}

fn main() {
    let mut args = env::args().skip(1);
    let addr = args
        .next()
        .or_else(|| env::var("SPG_ADDR").ok())
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = resolve_path(args.next(), "SPG_DB");
    let audit_path = resolve_path(args.next(), "SPG_AUDIT");
    let wal_path = resolve_path(args.next(), "SPG_WAL");
    // No CLI slot for the password — it lives in env only so it doesn't
    // leak into shell history / process listings (`ps`). Matches the
    // Valkey/Redis convention.
    let password = env::var("SPG_PASSWORD").ok().filter(|s| !s.is_empty());
    let limits = Limits {
        max_connections: parse_env_usize("SPG_MAX_CONNECTIONS"),
        max_query_rows: parse_env_usize("SPG_MAX_QUERY_ROWS"),
        max_query_bytes: parse_env_u64("SPG_MAX_QUERY_BYTES"),
        query_timeout_ms: parse_env_u64("SPG_QUERY_TIMEOUT_MS"),
        idle_timeout_sec: parse_env_u64("SPG_IDLE_TIMEOUT_SEC"),
        slow_query_log_ms: parse_env_u64("SPG_SLOW_QUERY_LOG_MS"),
        wal_min_free_bytes: parse_env_u64("SPG_WAL_MIN_FREE_BYTES"),
        shutdown_deadline_sec: parse_env_u64("SPG_SHUTDOWN_DEADLINE_SEC"),
    };
    install_shutdown_handlers();
    if let Err(e) = run(&addr, db_path, audit_path, wal_path, password, limits) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

/// Read a usize from `env_key`; non-positive / unparseable / unset
/// becomes `None` (= unlimited). Used by the v4.2 limits envs.
fn parse_env_usize(env_key: &str) -> Option<usize> {
    env::var(env_key)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

fn parse_env_u64(env_key: &str) -> Option<u64> {
    env::var(env_key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// v5.1: parse `SPG_PRELOAD_COLD_SEGMENT` into a list of
/// `(table, index, path)` specs. Format is a `;`-separated list
/// of `table:index:path` triples — e.g.
/// `users:by_id:/tmp/users.spg;orders:by_oid:/tmp/orders.spg`.
/// Malformed entries are logged and skipped; a fully empty / unset
/// env yields an empty Vec and the dispatch hot path stays no-op.
fn parse_cold_preload_env() -> Vec<ColdPreloadSpec> {
    let Ok(raw) = env::var("SPG_PRELOAD_COLD_SEGMENT") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.splitn(3, ':').collect();
        if parts.len() != 3 {
            eprintln!(
                "spg-server: SPG_PRELOAD_COLD_SEGMENT entry {entry:?} \
                 ignored — expected `table:index:path`"
            );
            continue;
        }
        let table = parts[0].trim().to_string();
        let index = parts[1].trim().to_string();
        let path = PathBuf::from(parts[2].trim());
        if table.is_empty() || index.is_empty() || path.as_os_str().is_empty() {
            eprintln!(
                "spg-server: SPG_PRELOAD_COLD_SEGMENT entry {entry:?} \
                 ignored — empty table / index / path"
            );
            continue;
        }
        out.push(ColdPreloadSpec {
            table,
            index,
            path,
            loaded: AtomicBool::new(false),
        });
    }
    if !out.is_empty() {
        eprintln!(
            "spg-server: cold-tier preload queue has {} spec(s); each one \
             will load on the first Op::Query after its table + index \
             both exist",
            out.len()
        );
    }
    out
}

/// v5.1: lazy cold-tier preload. Walks the spec queue; for each
/// unloaded spec where the target table + index both already
/// exist in the catalog, reads the segment bytes, registers the
/// segment via `Catalog::load_segment_bytes`, and wires every PK
/// in the segment as a `RowLocator::Cold` on the named index.
///
/// Short-circuits via `cold_preload_done` once every spec is
/// loaded so the dispatch hot path drops to one Relaxed load.
/// Errors don't fail the calling query — they're logged on
/// stderr and the spec stays pending for retry.
#[allow(
    clippy::too_many_lines,
    reason = "single-purpose preload routine; splitting hurts readability more than the line count helps"
)]
/// v6.1.4 — reconcile subscriber workers to the engine catalog.
/// Idempotent + cheap when nothing changed. Called at startup and
/// after any auto-commit that flipped `modified_catalog: true`.
///
/// Algorithm:
///   1. Snapshot wanted-state from `engine.subscriptions()`.
///   2. Workers in registry but not in catalog → flag shutdown +
///      drop the row.
///   3. Workers in catalog but not in registry → spawn a thread.
///
/// The registry is a `Mutex<BTreeMap<name, Arc<AtomicBool>>>`; the
/// flag is shared with the worker so signalling is lock-free.
/// v6.1.6 — read `<base>.cluster_id` (8 bytes LE) or, if absent,
/// generate a fresh random `u64` and persist it. The sidecar lives
/// alongside the WAL (or db file when no WAL is configured) so a
/// follower restart picks up the same `cluster_id` and cycle
/// detection survives across the bounce. Servers running with no
/// db_path AND no wal_path get an in-memory only `cluster_id` —
/// uniqueness is per-process, fine for ephemeral test workloads.
fn cluster_id_sidecar_path(wal_path: Option<&Path>, db_path: Option<&Path>) -> Option<PathBuf> {
    let base = wal_path.or(db_path)?;
    let mut name = base
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".cluster_id");
    Some(base.with_file_name(name))
}

fn load_or_generate_cluster_id(wal_path: Option<&Path>, db_path: Option<&Path>) -> u64 {
    if let Some(p) = cluster_id_sidecar_path(wal_path, db_path) {
        if p.exists()
            && let Ok(bytes) = std::fs::read(&p)
            && bytes.len() == 8
        {
            return u64::from_le_bytes(bytes.try_into().unwrap());
        }
        let id = generate_cluster_id();
        if let Err(e) = std::fs::write(&p, id.to_le_bytes()) {
            eprintln!(
                "spg-server: cluster_id sidecar write to {} failed: {e} — \
                 keeping in-memory id (cycle detection won't survive restart)",
                p.display()
            );
        }
        id
    } else {
        generate_cluster_id()
    }
}

/// Cheap non-crypto u64 mixing PID + wall-clock nanos. Good enough
/// for distinguishing 1000-server topologies; not a security
/// surface (no auth tokens derived from it). Lives entirely in
/// the host process.
fn generate_cluster_id() -> u64 {
    let pid = u64::from(std::process::id());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    // SplitMix64-shaped finaliser — spreads PID + time bits across
    // all 64 output bits.
    let mut x = ts.wrapping_mul(6364136223846793005).wrapping_add(pid);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

/// v6.2.1 — interval (milliseconds) between auto-analyze sweeps.
/// Defaults to 30 s. Tests set `SPG_AUTO_ANALYZE_INTERVAL_MS=200`
/// so the sweep fires within their probe window.
pub(crate) fn auto_analyze_interval_ms() -> u64 {
    std::env::var("SPG_AUTO_ANALYZE_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30_000)
}

/// v6.2.1 — spawn the background auto-analyze worker. One thread
/// per server. Sleeps in small ticks (200 ms) so the worker can
/// check for shutdown promptly even mid-interval.
pub(crate) fn spawn_auto_analyze_worker(state: Arc<ServerState>) {
    let interval = std::time::Duration::from_millis(auto_analyze_interval_ms());
    if interval.is_zero() {
        // Opt-out — interval 0 disables the worker entirely.
        return;
    }
    thread::Builder::new()
        .name("spg-auto-analyze".into())
        .spawn(move || {
            run_auto_analyze_loop(state, interval);
        })
        .ok();
}

const AUTO_ANALYZE_TICK: std::time::Duration = std::time::Duration::from_millis(200);

fn run_auto_analyze_loop(state: Arc<ServerState>, interval: std::time::Duration) {
    let mut last_sweep = std::time::Instant::now();
    loop {
        // Bounded sleep so a future shutdown signal (or
        // SIGTERM-driven exit) doesn't wait the full interval.
        thread::sleep(AUTO_ANALYZE_TICK);
        if last_sweep.elapsed() < interval {
            continue;
        }
        last_sweep = std::time::Instant::now();
        // Phase 1: snapshot the work-list under the read lock.
        let needs: Vec<String> = {
            let Ok(eng) = state.engine.read() else {
                continue;
            };
            eng.tables_needing_analyze()
        };
        if needs.is_empty() {
            continue;
        }
        // Phase 2: take the write lock once per table. Holding
        // briefly is critical — ANALYZE itself is fast on small
        // tables (sub-ms) and bounded on larger ones. A long
        // write-lock would block every other query.
        for table in &needs {
            let Ok(mut eng) = state.engine.write() else {
                break;
            };
            // The catalog may have changed since the read-lock
            // released (DROP TABLE, etc.) — re-check before
            // ANALYZE so we don't error-out a clean sweep.
            if eng.catalog().get(table).is_none() {
                continue;
            }
            if let Err(e) = eng.execute(&format!("ANALYZE {}", quote_ident_simple(table))) {
                eprintln!("spg-server: auto-analyze {table:?} failed: {e}");
            }
        }
    }
}

/// v6.2.1 — tiny SQL-ident quoter used by the auto-analyze worker
/// when composing `ANALYZE <name>`. Mirrors `spg_sql::ast::
/// quote_ident` behaviour but lives in the server crate so we
/// don't add a new spg-sql dependency just for one helper.
fn quote_ident_simple(name: &str) -> String {
    let needs_quote = name.is_empty()
        || name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        || name.starts_with(|c: char| c.is_ascii_digit());
    if needs_quote {
        let escaped: String = name
            .chars()
            .flat_map(|c| if c == '"' { vec!['"', '"'] } else { vec![c] })
            .collect();
        format!("\"{escaped}\"")
    } else {
        name.to_string()
    }
}

pub(crate) fn reconcile_subscriptions(state: &Arc<ServerState>) {
    use std::collections::BTreeMap;
    let want: BTreeMap<String, (String, bool)> = {
        let Ok(eng) = state.engine.read() else {
            return;
        };
        eng.subscriptions()
            .iter()
            .map(|(n, s)| (n.clone(), (s.conn_str.clone(), s.enabled)))
            .collect()
    };
    let Ok(mut workers) = state.sub_workers.lock() else {
        return;
    };
    // Step 1: tear down workers that are no longer wanted.
    let stale: Vec<String> = workers
        .keys()
        .filter(|n| !want.contains_key(n.as_str()))
        .cloned()
        .collect();
    for name in stale {
        if let Some(flag) = workers.remove(&name) {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    // Step 2: spawn workers for newly enabled subscriptions.
    for (name, (conn_str, enabled)) in &want {
        if !enabled {
            continue;
        }
        if workers.contains_key(name) {
            continue;
        }
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_worker = Arc::clone(&flag);
        let state_for_worker = Arc::clone(state);
        let name_clone = name.clone();
        let conn_clone = conn_str.clone();
        let thread_name = format!("spg-sub-{}", name.chars().take(20).collect::<String>());
        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                replication::run_subscription_worker(
                    name_clone,
                    conn_clone,
                    state_for_worker,
                    flag_for_worker,
                );
            })
            .ok();
        workers.insert(name.clone(), flag);
    }
}

pub(crate) fn try_lazy_preload_cold(state: &ServerState) {
    if state.cold_preload_done.load(Ordering::Relaxed) {
        return;
    }
    let mut still_pending = 0usize;
    for spec in &state.cold_preload {
        if spec.loaded.load(Ordering::Relaxed) {
            continue;
        }
        // Quick read-only probe: does (table, index) exist yet?
        let ready = {
            let Ok(engine) = state.engine.read() else {
                return;
            };
            let cat = engine.catalog();
            cat.get(&spec.table)
                .is_some_and(|t| t.indices().iter().any(|i| i.name == spec.index))
        };
        if !ready {
            still_pending += 1;
            continue;
        }
        let bytes = match std::fs::read(&spec.path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "spg-server: cold preload {}:{} from {} failed: {e}; \
                     marking loaded to avoid retry storm",
                    spec.table,
                    spec.index,
                    spec.path.display()
                );
                spec.loaded.store(true, Ordering::Relaxed);
                continue;
            }
        };
        let Ok(mut engine) = state.engine.write() else {
            return;
        };
        // Snapshot the catalog, register the segment, enumerate
        // its keys, and reinstall under one write lock so a
        // concurrent reader can't observe a partially-wired index.
        let mut cat = engine.catalog().clone();
        let seg_id = match cat.load_segment_bytes(bytes) {
            Ok(id) => id,
            Err(e) => {
                eprintln!(
                    "spg-server: cold preload {}:{} parse failed: {e}",
                    spec.table, spec.index
                );
                spec.loaded.store(true, Ordering::Relaxed);
                continue;
            }
        };
        let pairs: Vec<(spg_storage::IndexKey, spg_storage::RowLocator)> = {
            let Some(seg) = cat.cold_segment(seg_id) else {
                eprintln!(
                    "spg-server: cold preload {}:{} segment_id {seg_id} \
                     vanished after load — should be impossible",
                    spec.table, spec.index
                );
                spec.loaded.store(true, Ordering::Relaxed);
                continue;
            };
            seg.scan()
                .map(|(key, _payload)| {
                    (
                        spg_storage::IndexKey::Int(
                            i64::try_from(key).expect("cold-segment PK fits in i64"),
                        ),
                        spg_storage::RowLocator::Cold {
                            segment_id: seg_id,
                            page_offset: 0,
                        },
                    )
                })
                .collect()
        };
        let pairs_count = pairs.len();
        let Some(table_mut) = cat.get_mut(&spec.table) else {
            eprintln!(
                "spg-server: cold preload {}:{} table disappeared mid-load",
                spec.table, spec.index
            );
            spec.loaded.store(true, Ordering::Relaxed);
            continue;
        };
        if let Err(e) = table_mut.register_cold_locators(&spec.index, pairs) {
            eprintln!(
                "spg-server: cold preload {}:{} register_cold_locators failed: {e}",
                spec.table, spec.index
            );
            spec.loaded.store(true, Ordering::Relaxed);
            continue;
        }
        engine.replace_catalog(cat);
        spec.loaded.store(true, Ordering::Relaxed);
        // v5.3.1: record the segment_id → path mapping so a future
        // CHECKPOINT can emit it into the manifest. Best-effort:
        // mutex poisoning is logged and the loop continues —
        // legacy `SPG_PRELOAD_COLD_SEGMENT` still works without it.
        if let Ok(mut paths) = state.cold_segment_paths.lock() {
            paths.insert(seg_id, spec.path.clone());
        }
        eprintln!(
            "spg-server: cold preload {}:{} loaded {} row(s) from {}",
            spec.table,
            spec.index,
            pairs_count,
            spec.path.display()
        );
    }
    if still_pending == 0 {
        state.cold_preload_done.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_lines)] // startup wires snapshot+audit+WAL+bootstrap; splitting scatters init logic
fn run(
    addr: &str,
    db_path: Option<PathBuf>,
    audit_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    password: Option<String>,
    limits: Limits,
) -> std::io::Result<()> {
    // v5.3.1: pre-allocated path map so the manifest reader can
    // populate it before ServerState is built. After `run` finishes
    // setup it gets moved into `ServerState::cold_segment_paths`.
    let mut cold_segment_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();
    let mut manifest_wal_baseline: u64 = 0;
    let mut engine = match &db_path {
        Some(p) if p.exists() => {
            let bytes = fs::read(p)?;
            let path_str = p.display();
            // v4.1: snapshot may be either a v4.1 envelope (catalog +
            // users) or the bare v3.x catalog blob. `restore_envelope`
            // handles both transparently — v3.x files keep loading.
            let mut engine = Engine::restore_envelope(&bytes)
                .map_err(|e| std::io::Error::other(format!("restore from {path_str}: {e}")))?;
            eprintln!(
                "spg-server: restored {} table(s), {} user(s) from {path_str}",
                engine.catalog().table_count(),
                engine.users().len()
            );
            // v5.3.1: load the sidecar manifest, if any. Verifies the
            // snapshot CRC matches what we just read, then auto-
            // preloads every recorded cold-tier segment. Returns 0
            // (= legacy "replay from start") when no usable manifest
            // exists, so old deployments boot identical to v5.2.
            manifest_wal_baseline =
                load_manifest_and_preload_cold(&mut engine, p, &bytes, &mut cold_segment_paths);
            engine
        }
        Some(p) => {
            eprintln!(
                "spg-server: db file {} does not exist yet — starting fresh",
                p.display()
            );
            Engine::new()
        }
        None => Engine::new(),
    }
    .with_clock(wall_clock_micros)
    .with_salt_fn(urandom_salt_or_panic);

    if let Some(n) = limits.max_query_rows {
        engine = engine.with_max_query_rows(n);
    }

    let audit_log = match &audit_path {
        Some(p) if p.exists() => {
            let bytes = fs::read(p)?;
            let log = AuditLog::deserialize(&bytes).map_err(|e| {
                std::io::Error::other(format!("audit log {} rejected: {e}", p.display()))
            })?;
            eprintln!(
                "spg-server: verified audit log {} ({} entries)",
                p.display(),
                log.len()
            );
            log
        }
        Some(p) => {
            // Write a fresh header so the first append has somewhere to go.
            fs::write(p, AuditLog::header_bytes())?;
            eprintln!("spg-server: started fresh audit log at {}", p.display());
            AuditLog::new()
        }
        None => AuditLog::new(),
    };

    // Replay WAL onto the loaded snapshot. Truncated entries (server crash
    // mid-fsync) abort the loop with a warning rather than fatal — the
    // already-applied prefix wins, the trailing partial entry is dropped.
    // An open TX at end-of-WAL is auto-rolled-back.
    if let Some(p) = &wal_path
        && p.exists()
    {
        let mut bytes = fs::read(p)?;
        // v4.25 PITR: SPG_REPLAY_UPTO caps replay at a specific
        // byte offset of the WAL. Anything past that offset is
        // ignored on this boot — operator's restore mechanism.
        // 0 is a meaningful value (= "snapshot only, skip all WAL"),
        // so this parser doesn't reuse parse_env_u64's `n > 0` filter.
        if let Ok(s) = env::var("SPG_REPLAY_UPTO")
            && let Ok(upto) = s.trim().parse::<u64>()
        {
            let upto_usize = usize::try_from(upto).unwrap_or(usize::MAX);
            if bytes.len() > upto_usize {
                eprintln!(
                    "spg-server: PITR — truncating WAL replay at offset {upto} \
                     (of {} total bytes)",
                    bytes.len()
                );
                bytes.truncate(upto_usize);
            }
        }
        // v5.3.1: skip WAL bytes before the manifest's recorded
        // baseline. Those bytes have already been incorporated into
        // the snapshot we just restored — replaying them would
        // double-insert. v5.3.2 physically truncates the WAL file
        // up to the same offset for disk reclaim; v5.3.1 only
        // optimises replay time.
        let baseline_usize = usize::try_from(manifest_wal_baseline).unwrap_or(usize::MAX);
        if baseline_usize > 0 && baseline_usize <= bytes.len() {
            eprintln!(
                "spg-server: manifest skip — WAL replay starts at offset {manifest_wal_baseline} \
                 (of {} total bytes)",
                bytes.len()
            );
            bytes.drain(..baseline_usize);
        } else if baseline_usize > bytes.len() {
            // Manifest baseline is past EOF — the WAL file shrank
            // between checkpoint write and this boot. Defensive
            // fallback: replay the whole file. Data isn't lost,
            // just re-applied (and the auto-rollback at end-of-WAL
            // handles any mid-TX leftover).
            eprintln!(
                "spg-server: manifest WAL baseline {manifest_wal_baseline} exceeds file size {}; \
                 replaying from start as a safety net",
                bytes.len()
            );
        }
        let applied = replay_wal_bytes(&bytes, &mut engine)?;
        eprintln!(
            "spg-server: replayed {} WAL entries from {}",
            applied,
            p.display()
        );
        if engine.in_transaction() {
            eprintln!("spg-server: WAL ended mid-transaction — auto-rollback");
            engine
                .execute("ROLLBACK")
                .map_err(|e| std::io::Error::other(format!("post-replay rollback: {e}")))?;
        }
    } else if let Some(p) = &wal_path {
        // Create empty WAL file ahead of time so OpenOptions::append below
        // doesn't need a `create(true)` branch.
        fs::write(p, b"")?;
        eprintln!("spg-server: started fresh WAL at {}", p.display());
    }

    bootstrap_admin_from_env(&mut engine, db_path.as_deref())?;

    let (wal, wal_sync_clone) = match &wal_path {
        Some(p) => {
            let file = OpenOptions::new().append(true).open(p).map_err(|e| {
                std::io::Error::other(format!("open WAL {} for append: {e}", p.display()))
            })?;
            // v5.4.4 — clone the handle for lock-free fsync from the
            // async-commit flusher. `try_clone` failure (extremely
            // rare; would mean fd exhaustion at startup) degrades
            // gracefully: the flusher falls back to taking the mutex.
            let sync_clone = file.try_clone().ok().map(Arc::new);
            (Some(Mutex::new(file)), sync_clone)
        }
        None => (None, None),
    };

    let auth_msg = if password.is_some() {
        " (AUTH required)"
    } else {
        ""
    };
    let chaos = ChaosKnobs {
        wal_quota_bytes: parse_env_u64("SPG_FAIL_WAL_QUOTA_BYTES"),
        disable_wal_preflight: env::var("SPG_DISABLE_WAL_PREFLIGHT")
            .ok()
            .is_some_and(|s| !s.is_empty() && s != "0"),
    };
    let cold_preload = parse_cold_preload_env();
    let cold_preload_done = AtomicBool::new(cold_preload.is_empty());
    let hot_tier_byte_budget =
        parse_env_u64("SPG_HOT_TIER_BYTES").unwrap_or(DEFAULT_HOT_TIER_BYTES);
    let cluster_id = load_or_generate_cluster_id(wal_path.as_deref(), db_path.as_deref());
    let state = Arc::new(ServerState {
        engine: RwLock::new(engine),
        db_path,
        audit_log: Mutex::new(audit_log),
        audit_path,
        wal,
        wal_sync_clone,
        wal_path,
        commit_queue: Mutex::new(CommitQueueState {
            pending: VecDeque::new(),
            leader_active: false,
        }),
        password,
        limits,
        active_connections: AtomicUsize::new(0),
        metrics: Arc::new(observability::Metrics::default()),
        chaos,
        lag_state: Arc::new(replication::LagState::default()),
        cold_preload,
        cold_preload_done,
        hot_tier_byte_budget,
        cold_segment_paths: Mutex::new(cold_segment_paths),
        sub_workers: Mutex::new(BTreeMap::new()),
        cluster_id,
        wal_level: AtomicU8::new(parse_wal_level_env()),
        connections: RwLock::new(Vec::new()),
    });
    // v6.5.2 — register the global handle so the engine's
    // activity_provider callback can read the live registry. Safe
    // to set unconditionally: ACTIVITY_STATE is a OnceLock with
    // single-set semantics; subsequent server boots in the same
    // process (only relevant for tests) silently keep the first
    // state — engine refs through the static are always live.
    let _ = ACTIVITY_STATE.set(Arc::clone(&state));
    if let Ok(mut e) = state.engine.write() {
        // Replace the engine with one carrying the providers. The
        // builders consume by value, but we can swap in place by
        // taking ownership through std::mem::replace.
        let prev = std::mem::replace(&mut *e, Engine::new());
        *e = prev
            .with_activity_provider(activity_snapshot)
            .with_audit_providers(audit_chain_snapshot, audit_verify_snapshot);
    }

    // v6.1.4: spawn subscriber threads for any subscriptions
    // restored from the v4 snapshot envelope. Idempotent — if no
    // subscriptions exist (the common case), the call is a no-op.
    reconcile_subscriptions(&state);

    // v6.2.1: spawn the background auto-analyze worker. Single
    // thread per server — wakes every SPG_AUTO_ANALYZE_INTERVAL_MS
    // (default 30 s), reads the engine's `tables_needing_analyze()`
    // under a read-lock, then takes a write-lock per table to run
    // ANALYZE. The Acquire-load on the global shutdown atomic lets
    // the worker exit at server shutdown.
    spawn_auto_analyze_worker(Arc::clone(&state));

    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("spg-server: listening on {local}{auth_msg}");

    // v4.3: optional PG-wire compatibility listener. Opt-in via env
    // so a deployment that doesn't need psql / Metabase / DBeaver
    // doesn't pay the extra port + thread.
    if let Ok(pg_addr) = env::var("SPG_PG_ADDR")
        && !pg_addr.is_empty()
    {
        match pgwire::spawn_listener(&pg_addr, Arc::clone(&state)) {
            Ok(pg_local) => eprintln!("spg-server: pg-wire listening on {pg_local}"),
            Err(e) => eprintln!("spg-server: pg-wire failed to start on {pg_addr}: {e}"),
        }
    }

    // v4.13: optional observability HTTP endpoint. /healthz for
    // k8s liveness, /metrics for Prometheus scraping. The
    // listener reads live counters out of state directly via
    // an Arc<ServerState>.
    if let Ok(http_addr) = env::var("SPG_HTTP_ADDR")
        && !http_addr.is_empty()
    {
        match observability::spawn_http(&http_addr, Arc::clone(&state)) {
            Ok(http_local) => eprintln!("spg-server: http listening on {http_local}"),
            Err(e) => eprintln!("spg-server: http failed to start on {http_addr}: {e}"),
        }
    }

    // v4.24: optional master-side replication listener. When set,
    // followers can connect and stream the WAL.
    if let Ok(repl_addr) = env::var("SPG_REPL_ADDR")
        && !repl_addr.is_empty()
    {
        match replication::spawn_master_listener(&repl_addr, Arc::clone(&state)) {
            Ok(repl_local) => {
                eprintln!("spg-server: replication listening on {repl_local}");
            }
            Err(e) => {
                eprintln!("spg-server: replication failed to start on {repl_addr}: {e}");
            }
        }
    }

    // v4.24: optional follower mode. When set, the server tails
    // the master's WAL and applies it locally. Requires a db_path
    // and wal_path so the snapshot + WAL stream can be persisted
    // (and survive restart).
    if let Ok(master_addr) = env::var("SPG_FOLLOW_OF")
        && !master_addr.is_empty()
    {
        if let (Some(db), Some(wal)) = (state.db_path.clone(), state.wal_path.clone()) {
            let state_for_follower = Arc::clone(&state);
            thread::Builder::new()
                .name("spg-follower".into())
                .spawn(move || {
                    replication::run_follower(master_addr, db, wal, state_for_follower);
                })
                .ok();
            eprintln!("spg-server: started as follower");
        } else {
            eprintln!(
                "spg-server: SPG_FOLLOW_OF set but db_path or wal_path missing — \
                 follower mode requires both"
            );
        }
    }

    // v5.2.2: background freezer. Polls every tick; if hot-tier byte
    // sum exceeds `SPG_HOT_TIER_BYTES` (default 4 GiB), demotes a
    // batch of rows from the largest table with a BTree integer-PK
    // index. Opt-out via `SPG_FREEZER_DISABLE=1` for tests that
    // don't want background mutations under them.
    if freezer::spawn(Arc::clone(&state)).is_none() {
        eprintln!("spg-server: freezer disabled via SPG_FREEZER_DISABLE");
    }

    // v5.4.1: background flusher. Spawned only when async-commit
    // mode is opted in via `SPG_SYNCHRONOUS_COMMIT=off` (default is
    // synchronous — every WAL write already `sync_data`s, so the
    // flusher would be redundant). In async mode the flusher emits
    // a v5.4.0 `durability_checkpoint` WAL marker every
    // `SPG_FLUSHER_INTERVAL_US` µs (default 200 µs) so crash
    // recovery can identify how much of the async-commit window
    // had reached fsync at kill time.
    if flusher::spawn(Arc::clone(&state)).is_none() {
        // Default sync mode — silent. The opt-in async path logs
        // its own "async-commit on" banner when it lands in v5.4.2.
    }

    // v4.33 graceful shutdown: keep the blocking accept loop the
    // original code had (the per-connection timing is sensitive —
    // a polling listener changed the order in which max_connections
    // saw probe/handshake handlers release their slots and broke
    // `tests/e2e_limits::max_connections_*`). A dedicated wake-up
    // thread watches SHUTDOWN_FLAG and self-connects once when it
    // fires; that unblocks the next accept() and the loop's own
    // flag check breaks out cleanly.
    spawn_shutdown_waker(&listener)?;
    for stream in listener.incoming() {
        if SHUTDOWN_FLAG.load(Ordering::Acquire) {
            drop(stream); // close the wake-up socket without handling it
            break;
        }
        let mut stream = stream?;
        // v4.2 max_connections: try to claim a slot. On full, emit a
        // clear error frame and drop the socket. Doing the check
        // *after* accept costs us one extra accept+close per
        // overflow, but keeps the listener responsive to the
        // currently-allowed clients (an unbounded accept queue would
        // pile up).
        let guard = ConnectionGuard::try_claim(&state);
        let Some(guard) = guard else {
            let peer = stream.peer_addr().ok();
            let _ = write_frame(
                &mut stream,
                &build_error_response(&format!(
                    "max_connections reached ({} active)",
                    state.limits.max_connections.unwrap_or(0)
                )),
            );
            eprintln!("spg-server: rejected {peer:?}: max_connections reached");
            continue;
        };
        let state_for_thread = Arc::clone(&state);
        thread::spawn(move || {
            let _guard = guard; // released when this thread exits
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(stream, &state_for_thread) {
                eprintln!("spg-server: conn {peer:?}: {e}");
            }
        });
    }
    drain_connections(&state);
    Ok(())
}

/// v4.33: thread that watches `SHUTDOWN_FLAG` and, when it flips,
/// does a one-shot `connect(local_addr)` to wake the main thread's
/// blocking `accept()`. The main loop's own `SHUTDOWN_FLAG` check
/// then sees the flag set and breaks. The self-connection is
/// dropped immediately by the accept-side branch, so it never
/// consumes a `ConnectionGuard` slot or runs a handle thread.
fn spawn_shutdown_waker(listener: &TcpListener) -> std::io::Result<()> {
    let local = listener.local_addr()?;
    thread::Builder::new()
        .name("spg-shutdown-waker".into())
        .spawn(move || {
            while !SHUTDOWN_FLAG.load(Ordering::Acquire) {
                thread::sleep(SHUTDOWN_POLL);
            }
            let _ = TcpStream::connect(local);
        })?;
    Ok(())
}

/// v4.33: wait for in-flight connections to finish, bounded by
/// `SPG_SHUTDOWN_DEADLINE_SEC` (default 30 s). Polled by the main
/// thread after the accept loop breaks on `SHUTDOWN_FLAG`.
fn drain_connections(state: &ServerState) {
    let deadline_sec = state
        .limits
        .shutdown_deadline_sec
        .unwrap_or(DEFAULT_SHUTDOWN_DEADLINE_SEC);
    let started = Instant::now();
    let budget = Duration::from_secs(deadline_sec);
    eprintln!(
        "spg-server: shutdown signal received — draining {} connection(s), deadline {}s",
        state.active_connections.load(Ordering::Acquire),
        deadline_sec,
    );
    loop {
        let active = state.active_connections.load(Ordering::Acquire);
        if active == 0 {
            eprintln!("spg-server: drained — exiting 0");
            return;
        }
        if started.elapsed() >= budget {
            eprintln!(
                "spg-server: drain deadline hit with {active} connection(s) still active — exiting 0"
            );
            return;
        }
        thread::sleep(SHUTDOWN_POLL);
    }
}

/// v4.33: register SIGTERM/SIGINT handlers that flip the global
/// shutdown flag. Async-signal-safe: the handler does nothing but a
/// single relaxed atomic store. `libc::signal` returns the previous
/// handler; we ignore the result because we deliberately replace the
/// default `terminate` behavior with a graceful drain.
#[allow(unsafe_code)]
fn install_shutdown_handlers() {
    extern "C" fn handler(_sig: libc::c_int) {
        SHUTDOWN_FLAG.store(true, Ordering::Release);
    }
    // SAFETY: `signal(2)` is async-signal-safe to install. The
    // handler we register only performs an `AtomicBool::store`,
    // itself async-signal-safe (single-word atomic; no locks, no
    // allocation, no reentrancy). Installing the same handler for
    // SIGTERM + SIGINT means systemd's stop signal and a Ctrl-C in
    // the foreground both drive the same drain path.
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

/// RAII slot in the `active_connections` counter. `try_claim`
/// returns `None` when the configured `max_connections` cap is
/// already reached; otherwise it bumps the counter and the slot
/// frees on drop.
struct ConnectionGuard {
    state: Arc<ServerState>,
}

impl ConnectionGuard {
    fn try_claim(state: &Arc<ServerState>) -> Option<Self> {
        let max = state.limits.max_connections;
        loop {
            let current = state.active_connections.load(Ordering::Acquire);
            if let Some(cap) = max
                && current >= cap
            {
                return None;
            }
            if state
                .active_connections
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self {
                    state: Arc::clone(state),
                });
            }
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.active_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle(mut stream: TcpStream, state: &Arc<ServerState>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    // v4.5 idle timeout: when set, OS-level read timeout closes the
    // connection automatically. read() will return WouldBlock /
    // TimedOut after the budget; the outer loop translates that into
    // a clean exit.
    if let Some(secs) = state.limits.idle_timeout_sec {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(secs)));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];
    // v4.1: per-connection role.
    //   `None` = unauthenticated, must `AUTH` (legacy) or `AuthUser` first
    //   `Some(Role::Admin)` etc. = authenticated, dispatch enforces caps
    // Open mode (no SPG_PASSWORD + no users in catalog) starts as
    // `Some(Admin)`. Single-password mode starts as `None` and `Auth`
    // promotes to `Admin`. Multi-user mode (engine has users) starts as
    // `None` and only `AuthUser` is accepted.
    let mut role = initial_role(state)?;
    // v4.0: per-connection transaction state. BEGIN sets this to
    // true; COMMIT / ROLLBACK clear it. While true the dispatch path
    // takes the engine *write* lock for every statement on this
    // connection so the TX state stays consistent across reads and
    // writes inside the transaction.
    let mut in_tx = false;

    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            // v4.5: idle read timeout (or any explicit OS read
            // timeout) closes the connection cleanly.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                let _ = write_frame(
                    &mut stream,
                    &build_error_response("idle timeout reached, closing connection"),
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);

        loop {
            match decode(&buf) {
                Ok((frame, consumed)) => {
                    buf.drain(..consumed);
                    dispatch(&mut stream, &frame, state, &mut role, &mut in_tx)?;
                }
                Err(FrameError::ShortHeader | FrameError::ShortPayload) => break,
                Err(e) => {
                    let _ = write_frame(&mut stream, &build_error_response(&e.to_string()));
                    return Err(std::io::Error::other(e.to_string()));
                }
            }
        }
    }
}

/// Auth mode is decided per-connection at handshake time:
///
/// - engine has users → **multi-user RBAC**: start `None`, only
///   `Op::AuthUser` can authenticate.
/// - else `state.password` is set → **legacy single-password**: start
///   `None`, `Op::Auth` promotes to `Admin`.
/// - else → **open**: start `Some(Admin)`, every op allowed.
fn initial_role(state: &ServerState) -> std::io::Result<Option<Role>> {
    let has_users = {
        let engine = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        !engine.users().is_empty()
    };
    if has_users || state.password.is_some() {
        Ok(None)
    } else {
        Ok(Some(Role::Admin))
    }
}

/// Caller already passed the unauthenticated gate at the top of
/// `dispatch`; the panic is unreachable in practice but cheaper than
/// threading another error path through every write-gated branch.
fn current_role(role: Option<Role>) -> Role {
    role.expect("dispatch already gated on role.is_some()")
}

/// True for `CREATE USER ...` and `DROP USER ...` — the v4.1
/// admin-only DDL. Cheap byte peek (two case-insensitive idents
/// separated by whitespace). False negatives just route to the
/// regular write gate; false positives reject a query the user
/// would have been allowed to run, so we're conservative: only
/// match when both keywords are present in the expected order.
fn sql_is_user_mgmt(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    (lower.starts_with("create ") && lower["create ".len()..].trim_start().starts_with("user"))
        || (lower.starts_with("drop ") && lower["drop ".len()..].trim_start().starts_with("user"))
}

/// v4.34: true when the statement controls transaction boundaries
/// (`BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` /
/// `SAVEPOINT` / `RELEASE`). The auto-commit BEGIN..COMMIT wrap
/// must skip these — wrapping a client `BEGIN` would nest two
/// transactions; wrapping a `COMMIT`/`ROLLBACK` would tear down the
/// client's own TX before its body runs. Over-broad matches just
/// disable the wrap for that one statement (no correctness impact —
/// the original v4.30 preflight still gates the chaos path).
fn sql_is_tx_control(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    let first_word = lower
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or("");
    matches!(
        first_word,
        "begin" | "start" | "commit" | "rollback" | "savepoint" | "release" | "end"
    )
}

/// True for statements that mutate no engine state — exactly the set
/// `Engine::execute_readonly` accepts. Cheap byte peek (skip leading
/// whitespace, ASCII-case-fold the first alphabetic run); over-broad
/// hits (e.g. a column literally named "select") just take the read
/// path and engine returns `WriteRequired` if wrong — caller falls
/// back. False negatives are fine (cost: one extra write lock).
fn sql_is_read_only(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
    {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let kw = &bytes[start..i];
    matches!(kw.len(), 4 | 6) && {
        let mut lower = [0u8; 6];
        for (k, b) in kw.iter().enumerate() {
            lower[k] = b.to_ascii_lowercase();
        }
        let s = &lower[..kw.len()];
        s == b"select" || s == b"show"
    }
}

/// v6.1.8 — `effective_wal_level` discriminant. `replica`
/// (default) and `logical` are the only legal values, matching
/// PG semantics. Stored as a `u8` in `ServerState::wal_level`
/// so reads are lock-free; transitions go through `SET`.
pub(crate) const WAL_LEVEL_REPLICA: u8 = 0;
pub(crate) const WAL_LEVEL_LOGICAL: u8 = 1;

/// v6.1.8 — parse the `SPG_WAL_LEVEL` env var at startup.
/// Defaults to `replica` on absence or unknown value (loud
/// warning so a typo doesn't silently downgrade).
fn parse_wal_level_env() -> u8 {
    match std::env::var("SPG_WAL_LEVEL")
        .ok()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") | Some("replica") => WAL_LEVEL_REPLICA,
        Some("logical") => WAL_LEVEL_LOGICAL,
        Some(other) => {
            eprintln!(
                "spg-server: SPG_WAL_LEVEL={other:?} unknown — defaulting to replica. \
                 Valid values: replica, logical"
            );
            WAL_LEVEL_REPLICA
        }
    }
}

/// v6.1.8 — render the current wal_level as the SQL-surface
/// string. Used by `SHOW effective_wal_level`.
pub(crate) fn wal_level_label(v: u8) -> &'static str {
    match v {
        WAL_LEVEL_LOGICAL => "logical",
        _ => "replica",
    }
}

/// v6.1.8 — cheap prefix-match for `SET effective_wal_level`.
fn sql_looks_like_set_wal_level(sql: &str) -> bool {
    let trimmed = sql.trim_start().to_ascii_lowercase();
    trimmed.starts_with("set effective_wal_level")
}

/// v6.1.8 — cheap prefix-match for `SHOW effective_wal_level`.
fn sql_looks_like_show_wal_level(sql: &str) -> bool {
    let trimmed = sql.trim_start().to_ascii_lowercase();
    trimmed == "show effective_wal_level"
        || trimmed.starts_with("show effective_wal_level ")
        || trimmed.starts_with("show effective_wal_level;")
}

/// v6.1.8 — extract the value side of `SET effective_wal_level = '<v>'`.
/// Trims surrounding quotes (PG-style) and case-folds. Returns
/// `Err(msg)` for malformed input.
fn parse_set_wal_level_value(sql: &str) -> Result<u8, String> {
    let lower = sql.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("set effective_wal_level")
        .ok_or_else(|| "expected `set effective_wal_level …`".to_string())?
        .trim_start();
    // Accept `=` or `to` between the name and the value.
    let val_part = if let Some(r) = rest.strip_prefix('=') {
        r.trim()
    } else if let Some(r) = rest.strip_prefix("to ") {
        r.trim()
    } else {
        return Err("expected `=` or `TO` after effective_wal_level".to_string());
    };
    let value = val_part
        .trim_matches(|c: char| matches!(c, '\'' | '"' | ';'))
        .trim();
    match value {
        "replica" => Ok(WAL_LEVEL_REPLICA),
        "logical" => Ok(WAL_LEVEL_LOGICAL),
        other => Err(format!(
            "unknown effective_wal_level {other:?}; expected `replica` or `logical`"
        )),
    }
}

/// v6.1.8 — handler for the SET intercept. Updates the global
/// `wal_level` atomic and emits CommandComplete.
fn handle_set_wal_level(
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    sql: &str,
) -> std::io::Result<()> {
    match parse_set_wal_level_value(sql) {
        Ok(level) => {
            state.wal_level.store(level, Ordering::Release);
            emit_result(
                stream,
                Ok(spg_engine::QueryResult::CommandOk {
                    affected: 1,
                    modified_catalog: false,
                }),
            )
        }
        Err(msg) => write_frame(stream, &build_error_response(&msg)),
    }
}

/// v6.1.8 — handler for the SHOW intercept. Returns a single
/// row `(effective_wal_level TEXT NOT NULL)`.
fn handle_show_wal_level(
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
) -> std::io::Result<()> {
    let level = state.wal_level.load(Ordering::Acquire);
    let row = vec![Row::new(vec![Value::Text(
        wal_level_label(level).to_string(),
    )])];
    let columns = vec![ColumnSchema::new(
        "effective_wal_level",
        DataType::Text,
        false,
    )];
    emit_result(stream, Ok(spg_engine::QueryResult::Rows { columns, rows: row }))
}

/// v6.1.7 — cheap prefix-match for `WAIT FOR`. The wire-layer
/// intercept only re-parses the SQL when this returns true, so
/// the cost on every non-WAIT query is a tiny first-word scan.
fn sql_looks_like_wait_for(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    if trimmed.len() < 4 {
        return false;
    }
    trimmed.as_bytes()[..4]
        .iter()
        .zip(b"WAIT")
        .all(|(a, b)| a.to_ascii_uppercase() == *b)
        && trimmed
            .as_bytes()
            .get(4)
            .is_some_and(u8::is_ascii_whitespace)
}

/// v6.1.7 — WAIT FOR WAL POSITION handler. Polls
/// `lag_state.follower_applied_pos` at 5 ms cadence until the
/// target is reached or the optional timeout elapses. Returns
/// CommandComplete with `affected=1` on reach, `affected=0` on
/// timeout — clients distinguish the two via the count.
fn handle_wait_for_wal_position(
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    target: u64,
    timeout_ms: Option<u64>,
) -> std::io::Result<()> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(5);
    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    loop {
        let current = state
            .lag_state
            .follower_applied_pos
            .load(Ordering::Acquire);
        if current >= target {
            return emit_result(
                stream,
                Ok(spg_engine::QueryResult::CommandOk {
                    affected: 1,
                    modified_catalog: false,
                }),
            );
        }
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            return emit_result(
                stream,
                Ok(spg_engine::QueryResult::CommandOk {
                    affected: 0,
                    modified_catalog: false,
                }),
            );
        }
        std::thread::sleep(POLL);
    }
}

#[allow(clippy::too_many_lines)] // big dispatch table, splitting would scatter the per-op gates
fn dispatch(
    stream: &mut TcpStream,
    frame: &Frame,
    state: &Arc<ServerState>,
    role: &mut Option<Role>,
    in_tx: &mut bool,
) -> std::io::Result<()> {
    // Gate every non-Ping / non-Auth op until the connection has
    // authenticated. Ping stays accessible so health probes still work
    // (matches Valkey/Redis policy).
    if role.is_none() && !matches!(frame.op, Op::Ping | Op::Auth | Op::AuthUser) {
        return write_frame(
            stream,
            &build_error_response("authentication required: send AUTH first"),
        );
    }
    match frame.op {
        Op::Ping => write_frame(stream, &Frame::pong()),
        Op::Auth => {
            let candidate = match parse_auth(frame) {
                Ok(s) => s,
                Err(e) => return write_frame(stream, &build_error_response(&e.to_string())),
            };
            // v4.1: legacy `AUTH <password>` only makes sense in
            // single-password mode. Once the engine has users, force
            // clients onto `AuthUser` so a per-user password can't
            // accidentally be reused as the global one.
            let users_exist = state.engine.read().is_ok_and(|e| !e.users().is_empty());
            if users_exist {
                return write_frame(
                    stream,
                    &build_error_response("RBAC active: use AUTH USER <name> <password>"),
                );
            }
            // Constant-time compare is overkill for a local Docker
            // sidecar — a `==` here is fine in our threat model. If the
            // server has no password configured we still accept AUTH
            // gracefully (Redis-like): any password matches "no
            // password" with a 'no-op' Pong, so clients written for
            // the auth flow keep working against open instances.
            let ok = match &state.password {
                Some(pw) => candidate == pw,
                None => true,
            };
            if ok {
                *role = Some(Role::Admin);
                write_frame(stream, &Frame::pong())
            } else {
                write_frame(stream, &build_error_response("AUTH: wrong password"))
            }
        }
        Op::AuthUser => {
            let (user, pw) = match parse_auth_user(frame) {
                Ok(t) => t,
                Err(e) => return write_frame(stream, &build_error_response(&e.to_string())),
            };
            let verified = state
                .engine
                .read()
                .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?
                .verify_user(user, pw);
            match verified {
                Some(r) => {
                    *role = Some(r);
                    write_frame(stream, &Frame::pong())
                }
                None => write_frame(stream, &build_error_response("AUTH: invalid credentials")),
            }
        }
        Op::Stats => {
            let body = render_stats(state)?;
            write_frame(stream, &build_stats_response(&body))
        }
        Op::Query => {
            state.metrics.queries_total.fetch_add(1, Ordering::Relaxed);
            // v5.1: cold-tier preload — checks each pending spec for
            // (table, index) existence and loads on the first hit.
            // No-op once every spec has loaded (Relaxed bool).
            try_lazy_preload_cold(state);
            let sql = match parse_query(frame) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    state.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return write_frame(stream, &build_error_response(&e.to_string()));
                }
            };
            // v4.33 slow-query log: scoped guard times the entire
            // dispatch (read-path, write-path, every error branch) and
            // emits one JSON line on stderr if elapsed exceeds
            // `SPG_SLOW_QUERY_LOG_MS`. Drop runs on every return below.
            let _slow_log = SlowLogGuard::new(state, &sql, *role);
            // v6.1.7 — server-layer intercept for WAIT FOR WAL POSITION.
            // The engine refuses this statement; we read `lag_state`
            // (which the engine has no access to) and poll until the
            // target is reached or the optional timeout fires.
            if sql_looks_like_wait_for(&sql)
                && let Ok(stmt) = spg_sql::parser::parse_statement(&sql)
                && let spg_sql::ast::Statement::WaitForWalPosition { pos, timeout_ms } = stmt
            {
                return handle_wait_for_wal_position(stream, state, pos, timeout_ms);
            }
            // v6.1.8 — server-layer intercept for
            //   SET   effective_wal_level = 'logical' | 'replica'
            //   SHOW  effective_wal_level
            // wal_level is global server state, not a session var,
            // so the engine's pgwire-style session-settings map
            // isn't the right home for it.
            if sql_looks_like_show_wal_level(&sql) {
                return handle_show_wal_level(stream, state);
            }
            if sql_looks_like_set_wal_level(&sql) {
                return handle_set_wal_level(stream, state, &sql);
            }
            // v4.0 fast path: SELECT / SHOW outside an active TX take
            // the engine *read* lock and run in parallel with other
            // readers. WriteRequired drop-through is rare (only if
            // `sql_is_read_only` peek mis-classifies — over-broad
            // matches like a column named "select" don't happen in
            // practice).
            if !*in_tx && sql_is_read_only(&sql) {
                // v4.5: per-query cancellation token. Watchdog
                // thread (if SPG_QUERY_TIMEOUT_MS set) trips the
                // flag after the budget; the engine's row loops
                // poll it at checkpoints and bail.
                let cancel_flag = Arc::new(AtomicBool::new(false));
                let watchdog = spawn_query_watchdog(state, &cancel_flag);
                let engine = state
                    .engine
                    .read()
                    .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
                let budget = usize::try_from(
                    state
                        .limits
                        .max_query_bytes
                        .unwrap_or(DEFAULT_MAX_QUERY_BYTES),
                )
                .unwrap_or(usize::MAX);
                alloc_budget::reset_query_budget(budget, &cancel_flag);
                let result = engine.execute_readonly_with_cancel(
                    &sql,
                    spg_engine::CancelToken::from_flag(&cancel_flag),
                );
                alloc_budget::clear_query_budget();
                drop(engine);
                watchdog.cancel();
                if !matches!(&result, Err(EngineError::WriteRequired)) {
                    return emit_result(stream, result);
                }
            }
            // v4.1: anything that falls through to the write path
            // requires a role with write privileges. ReadOnly users
            // hit this gate; admin / readwrite proceed. CREATE USER
            // / DROP USER need the stricter Admin role.
            let acting = current_role(*role);
            if sql_is_user_mgmt(&sql) {
                if !acting.can_manage_users() {
                    return write_frame(
                        stream,
                        &build_error_response(
                            "permission denied: user management requires admin role",
                        ),
                    );
                }
            } else if !acting.can_write() {
                return write_frame(
                    stream,
                    &build_error_response(
                        "permission denied: write requires admin or readwrite role",
                    ),
                );
            }
            // v4.25: intercept BACKUP TO '<path>' [INCREMENTAL SINCE <n>]
            // before passing to the engine. Admin-only — backup writes
            // arbitrary file paths so it lives behind the same gate as
            // user management.
            if let Some(backup_intent) = parse_backup_intent(&sql) {
                if !acting.can_manage_users() {
                    return write_frame(
                        stream,
                        &build_error_response("permission denied: BACKUP requires admin role"),
                    );
                }
                return run_backup_command(stream, state, &backup_intent);
            }
            // v5.3.2: intercept CHECKPOINT. Admin-only because it
            // writes the snapshot + manifest + truncates the WAL —
            // same surface as BACKUP / user management.
            if parse_checkpoint_intent(&sql) {
                if !acting.can_manage_users() {
                    return write_frame(
                        stream,
                        &build_error_response("permission denied: CHECKPOINT requires admin role"),
                    );
                }
                return run_checkpoint_command(stream, state);
            }
            // v4.34: when WAL is on and this is an auto-commit write
            // (no client-driven TX in flight, not a TX-control verb),
            // wrap the engine mutation in an implicit BEGIN..COMMIT.
            // v4.41 replaces the original three-v2-record block with
            // a single v3 `auto_commit_sql` record — same atomicity
            // (one write_all + one fsync), 35→9 header bytes per
            // write. If the WAL append fails, we ROLLBACK the
            // implicit TX — the live in-memory state never sees the
            // half-applied write. Closes the real ENOSPC mid-
            // `write_all` window that v4.30's preflight chaos path
            // couldn't fix on its own (PROD_READY 1.11).
            let needs_wrap = !*in_tx && state.wal.is_some() && !sql_is_tx_control(&sql);
            // v4.30 preflight (chaos path): if SPG_FAIL_WAL_QUOTA_BYTES
            // is set and the block won't fit, reject before any engine
            // mutation so even without the wrap, the in-memory state
            // stays in sync. Skipped when the test deliberately turns
            // it off via SPG_DISABLE_WAL_PREFLIGHT — that path forces
            // the v4.34 rollback to be exercised end-to-end.
            if let Some(quota) = state.chaos.wal_quota_bytes
                && let Some(wal_path) = &state.wal_path
                && !state.chaos.disable_wal_preflight
            {
                let cur = fs::metadata(wal_path).map_or(0, |m| m.len());
                let needed = if needs_wrap {
                    wal_v3_auto_commit_size(&sql)
                } else {
                    4 + sql.len() as u64
                };
                if cur.saturating_add(needed) > quota {
                    return write_frame(
                        stream,
                        &build_error_response(&format!(
                            "wal quota exceeded: cur={cur} + {needed} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)"
                        )),
                    );
                }
            }
            let cancel_flag = Arc::new(AtomicBool::new(false));
            let watchdog = spawn_query_watchdog(state, &cancel_flag);
            // v4.42 — split the wrap path from the non-wrap path.
            //
            // **Wrap path** (auto-commit write, WAL on): push the
            // SQL onto the commit-barrier queue and wait on the
            // task's `ack` channel. The first arriving task flips
            // `leader_active` and drives `run_leader_commit_round`
            // (drain → batched fsync → install/rollback), then
            // acks every task in the group. Group of 1 = the
            // pusher is itself the leader and proceeds without
            // any condvar wait — same latency shape as v4.41.1.
            //
            // **Non-wrap path** (TX-control verbs or writes
            // inside an explicit client TX): keep the v4.41.1
            // synchronous flow. These don't fan out, so the
            // commit barrier would only add coordination cost.
            // The legacy v2 WAL framing is the right format here
            // (auto-commit framing assumes there's no client TX
            // in flight, which this branch contradicts).
            let (result, wal_result, snapshot) = if needs_wrap {
                let (ack_tx, ack_rx) = mpsc::sync_channel::<CommitResult>(1);
                let task = CommitTask {
                    sql: sql.clone(),
                    cancel_flag: Arc::clone(&cancel_flag),
                    ack: ack_tx,
                };
                let became_leader = enqueue_commit_task(state, task);
                if became_leader {
                    run_leader_commit_round(state);
                }
                let CommitResult {
                    result,
                    wal_outcome,
                } = ack_rx.recv().map_err(|_| {
                    std::io::Error::other(
                        "commit barrier: ack channel closed before result arrived",
                    )
                })?;
                // Wrap path always has WAL on (see `needs_wrap`
                // gate above), so the wal-off snapshot branch is
                // unreachable here. Auto-commit wraps never leave
                // a TX open, so `*in_tx` would already be false —
                // sync it explicitly anyway against the engine
                // state so a hypothetical engine-internal
                // mismatch can't drift.
                *in_tx = state.engine.read().is_ok_and(|e| e.in_transaction());
                (result, wal_outcome, None)
            } else {
                let mut engine = state
                    .engine
                    .write()
                    .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
                let budget = usize::try_from(
                    state
                        .limits
                        .max_query_bytes
                        .unwrap_or(DEFAULT_MAX_QUERY_BYTES),
                )
                .unwrap_or(usize::MAX);
                alloc_budget::reset_query_budget(budget, &cancel_flag);
                let result = engine
                    .execute_with_cancel(&sql, spg_engine::CancelToken::from_flag(&cancel_flag));
                alloc_budget::clear_query_budget();
                let was_command_ok = matches!(result, Ok(QueryResult::CommandOk { .. }));
                let wal_result = if was_command_ok && state.wal.is_some() {
                    append_wal(state, &sql)
                } else {
                    Ok(())
                };
                *in_tx = engine.in_transaction();
                let snapshot = if state.db_path.is_some() && state.wal.is_none() {
                    match &result {
                        Ok(QueryResult::CommandOk {
                            modified_catalog: true,
                            ..
                        }) => Some(engine.snapshot()),
                        _ => None,
                    }
                } else {
                    None
                };
                drop(engine);
                (result, wal_result, snapshot)
            };
            watchdog.cancel();
            // Snapshot the catalog first; an audit entry that survives a
            // partial flush would be inconsistent.
            if let (Some(bytes), Some(path)) = (snapshot.as_ref(), state.db_path.as_deref())
                && let Err(e) = write_atomic(path, bytes)
            {
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("snapshot write failed: {e}")),
                );
                return Err(e);
            }
            // v5.3.1 — sidecar manifest write. Best-effort: a
            // manifest failure here doesn't kill the snapshot (the
            // WAL is still the durability surface; legacy SPG_PRELOAD
            // _COLD_SEGMENT keeps working when the manifest is
            // missing). Only fires when a snapshot was actually
            // written (no-WAL mode `modified_catalog: true`).
            if let (Some(bytes), Some(path)) = (snapshot.as_ref(), state.db_path.as_deref()) {
                let paths_snapshot = state
                    .cold_segment_paths
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let wal_len = state
                    .wal_path
                    .as_deref()
                    .and_then(|p| fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                write_manifest_alongside(path, bytes, &paths_snapshot, wal_len);
            }
            if let Err(e) = wal_result {
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("WAL append failed: {e}")),
                );
                return Err(e);
            }
            // Audit-log only when the committed state actually changed AND
            // an audit path is configured. v3.4.0 fix: previously the
            // in-memory AuditLog grew every write even without an audit
            // file (the SQL text was cloned into the log forever), so a
            // long-running server with no audit configured still leaked
            // a few MB per 10K writes.
            if state.audit_path.is_some()
                && matches!(
                    result,
                    Ok(QueryResult::CommandOk {
                        modified_catalog: true,
                        ..
                    })
                )
                && let Err(e) = append_audit(state, &sql)
            {
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("audit append failed: {e}")),
                );
                return Err(e);
            }
            // v6.1.4 — CREATE / DROP SUBSCRIPTION flips
            // `modified_catalog: true`. Reconcile picks up the
            // change and spawns / tears down the corresponding
            // worker thread. Idempotent + cheap when the catalog
            // change wasn't subscription-related.
            if matches!(
                result,
                Ok(QueryResult::CommandOk {
                    modified_catalog: true,
                    ..
                })
            ) {
                reconcile_subscriptions(state);
            }
            emit_result(stream, result)
        }
        Op::Pong
        | Op::RowDescription
        | Op::DataRow
        | Op::DataRowBatch
        | Op::CommandComplete
        | Op::ErrorResponse
        | Op::StatsResponse => write_frame(
            stream,
            &Frame::error("client → server opcode not accepted on this side"),
        ),
        Op::Error => write_frame(
            stream,
            &Frame::error("clients should not send Error frames"),
        ),
    }
}

/// Append one WAL entry: `[u32 LE sql_len][sql_bytes]`. Fsync'd before
/// returning so a successful `CommandComplete` on the wire reflects durable
/// state. Caller is expected to hold the engine lock around `execute()`,
/// release it, then call this — there's no need to keep both locks held.
/// Render a `key=value`-per-line summary of server state for the Stats opcode.
/// Acquires the engine and audit locks; intentionally cheap (no per-table
/// row walk beyond `row_count()`).
fn render_stats(state: &ServerState) -> std::io::Result<String> {
    use std::fmt::Write as _;
    let engine = state
        .engine
        .read()
        .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
    let audit = state
        .audit_log
        .lock()
        .map_err(|_| std::io::Error::other("audit mutex poisoned"))?;
    let catalog = engine.catalog();

    let mut out = String::new();
    writeln!(out, "spg_version={}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(out, "tables={}", catalog.table_count()).unwrap();
    for i in 0..catalog.table_count() {
        // The catalog doesn't currently expose iteration directly; walk via
        // table name lookup via successive name(). It does have `.get()` but
        // not an iterator API. For v1.0 we ship the simplest stats.
        // Catalog has private `tables` field — use `get_*` on names…
        // Actually we have no name iterator. Skip per-table breakdown; emit
        // total row count instead.
        let _ = i; // suppress unused; placeholder until catalog grows iter.
    }
    // Total rows: walk via the public API. There is no rows-iterator on
    // Catalog yet; for v1.0 stats we report total table count and audit /
    // wal facts. Per-table row counts will require a Catalog::table_names()
    // addition — left for v1.1.
    writeln!(out, "in_transaction={}", engine.in_transaction()).unwrap();
    writeln!(out, "audit_entries={}", audit.len()).unwrap();
    writeln!(
        out,
        "db_path={}",
        state
            .db_path
            .as_deref()
            .map_or("<in-memory>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    writeln!(
        out,
        "audit_path={}",
        state
            .audit_path
            .as_deref()
            .map_or("<disabled>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    writeln!(
        out,
        "wal_path={}",
        state
            .wal_path
            .as_deref()
            .map_or("<disabled>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    Ok(out)
}

/// v4.25: parse `BACKUP TO '<path>'` and
/// `BACKUP TO '<path>' INCREMENTAL SINCE <n>` from the raw SQL
/// text. Returns None if the statement isn't a backup. Preserves
/// the path's original case (the lowercased form is only used to
/// recognise keywords).
fn parse_backup_intent(sql: &str) -> Option<BackupIntent> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let lower = trimmed.to_ascii_lowercase();
    let after_prefix = lower
        .strip_prefix("backup ")?
        .trim_start()
        .strip_prefix("to ")?
        .trim_start();
    let prefix_consumed = lower.len() - after_prefix.len();
    if !trimmed[prefix_consumed..].starts_with('\'') {
        return None;
    }
    let after_open = &trimmed[prefix_consumed + 1..];
    let close = after_open.find('\'')?;
    let path = after_open[..close].to_string();
    let tail = after_open[close + 1..].trim().to_ascii_lowercase();
    if tail.is_empty() {
        return Some(BackupIntent::Full { path });
    }
    let since_str = tail
        .strip_prefix("incremental ")?
        .trim_start()
        .strip_prefix("since ")?
        .trim_start();
    let since: u64 = since_str.parse().ok()?;
    Some(BackupIntent::Incremental { path, since })
}

#[derive(Debug)]
enum BackupIntent {
    Full { path: String },
    Incremental { path: String, since: u64 },
}

/// v5.3.2: parse the `CHECKPOINT` keyword. No arguments, no
/// variations — the SQL form is intentionally minimal because the
/// operation always means the same thing: snapshot the engine,
/// write the manifest, truncate the WAL.
fn parse_checkpoint_intent(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    trimmed.eq_ignore_ascii_case("checkpoint")
}

/// v5.3.2 — `CHECKPOINT` handler. Writes a fresh snapshot to
/// `db_path`, an updated manifest to the sibling
/// `<db>.spg/manifest.v10`, and truncates the WAL file to 0 bytes.
/// The next boot loads the manifest, preloads every cold segment,
/// and starts WAL replay from byte 0 (which is now empty until the
/// next post-checkpoint write).
///
/// Single-fsync semantics: snapshot → manifest → WAL truncate is a
/// strict order. A crash between any two of those leaves the
/// system in a state the boot path can detect (snapshot CRC vs
/// manifest's `catalog_crc32`) and falls back to legacy
/// snapshot+WAL-from-0 replay. v5.3.x intentionally doesn't add a
/// CHECKPOINT WAL record; v5.4 manifest-with-WAL-coordination is
/// a separate trigger.
fn run_checkpoint_command(stream: &mut TcpStream, state: &ServerState) -> std::io::Result<()> {
    let Some(db_path) = state.db_path.as_deref() else {
        return write_frame(
            stream,
            &build_error_response("CHECKPOINT requires a db_path (server started without one)"),
        );
    };
    // Acquire write lock so no concurrent mutation can land between
    // snapshot capture and WAL truncate.
    let snapshot_bytes = {
        let engine = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        if engine.in_transaction() {
            return write_frame(
                stream,
                &build_error_response("CHECKPOINT refused: an open transaction is in flight"),
            );
        }
        let bytes = engine.snapshot();
        drop(engine);
        bytes
    };
    if let Err(e) = write_atomic(db_path, &snapshot_bytes) {
        return write_frame(
            stream,
            &build_error_response(&format!("CHECKPOINT snapshot write failed: {e}")),
        );
    }
    let cold_paths = state
        .cold_segment_paths
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    // Post-truncate the WAL will start at byte 0, so the manifest's
    // `wal_baseline_offset` for the *next* boot is also 0 — every
    // byte after this point is post-checkpoint and must be replayed.
    write_manifest_alongside(db_path, &snapshot_bytes, &cold_paths, 0);
    // Truncate WAL last — until the manifest lands, a crash here
    // would leave the WAL holding old bytes that the manifest CRC
    // check will detect on the next boot.
    if let Some(wal_mutex) = state.wal.as_ref() {
        let wal_lock = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("WAL mutex poisoned"))?;
        if let Err(e) = wal_lock.set_len(0) {
            // Best-effort: log + report. The snapshot + manifest
            // already landed; on next boot the manifest's CRC will
            // match and the residual WAL bytes will replay as a
            // (defensive) no-op idempotency replay. Not a hard
            // failure.
            return write_frame(
                stream,
                &build_error_response(&format!("CHECKPOINT WAL truncate failed: {e}")),
            );
        }
        if let Err(e) = wal_lock.sync_data() {
            return write_frame(
                stream,
                &build_error_response(&format!("CHECKPOINT WAL sync failed: {e}")),
            );
        }
        drop(wal_lock);
    }
    // Return 0 in the affected-rows slot — there's no natural row
    // count for a checkpoint. Operators can poll `wal_path` size
    // afterwards to confirm the truncate.
    write_frame(stream, &build_command_complete(0))
}

fn run_backup_command(
    stream: &mut TcpStream,
    state: &ServerState,
    intent: &BackupIntent,
) -> std::io::Result<()> {
    let result = match intent {
        BackupIntent::Full { path } => backup::take_full_backup(state, Path::new(path)),
        BackupIntent::Incremental { path, since } => {
            backup::take_incremental_backup(state, Path::new(path), *since)
        }
    };
    match result {
        // Re-use the existing `affected rows` slot to ship the
        // captured WAL position back to the caller — it's the
        // number an incremental backup will pass as SINCE.
        Ok(wal_pos) => write_frame(stream, &build_command_complete(wal_pos)),
        Err(e) => write_frame(
            stream,
            &build_error_response(&format!("backup failed: {e}")),
        ),
    }
}

/// WAL record format (sentinel-bit framing across versions):
///   v1 (≤ v4.36): `[u32 LE len][len bytes]`                                bit 31 = 0
///   v2 (v4.37+):  `[u32 LE (len | 0x8000_0000)][u32 LE crc32][len bytes]`  bit 31 = 1, bit 30 = 0
///   v3 (v4.41+):  `[u32 LE (len | 0xC000_0000)][u32 LE crc32][1 byte type][len bytes payload]`
///                                                                          bit 31 = 1, bit 30 = 1
///
/// v1 lengths are << 2 GiB in practice so bit 31 was free for the
/// v2 sentinel; v2 lengths are << 1 GiB in practice so bit 30 was
/// free for v3. `len` in the v3 frame counts only the `payload`
/// body (the leading type byte is fixed header overhead, kept out
/// of `len` so the quota math stays simple).
///
/// The CRC32 in v3 covers `[type byte || payload]` — the type byte
/// is integrity-protected too. Unknown type bytes during replay
/// return a hard error (no silent skip).
///
/// Old v4.x binaries reading v3 records crash on the "huge len" —
/// forward-compat isn't required by STABILITY (clients only need
/// to read older formats).
pub(crate) const WAL_V2_SENTINEL: u32 = 0x8000_0000;
pub(crate) const WAL_V3_FLAG: u32 = 0x4000_0000;
pub(crate) const WAL_V3_SENTINEL: u32 = WAL_V2_SENTINEL | WAL_V3_FLAG;

/// v5.4.2 — cached `SPG_SYNCHRONOUS_COMMIT` parse. Returns `true`
/// when async-commit mode is opted in (`SPG_SYNCHRONOUS_COMMIT` ∈
/// {`off`, `false`, `0`}, case-insensitive). The result is cached
/// behind `OnceLock` because the env is read once per process; a
/// benchmark that flips the knob must restart the server.
///
/// In async mode the WAL write path skips `sync_data` — the
/// flusher thread (v5.4.1) handles durability via periodic
/// `durability_checkpoint` markers. The opt-in keyword set is
/// the same one `FlusherConfig::from_env` recognises, so a
/// misread env stays consistent across both modules.
pub(crate) fn synchronous_commit_disabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SPG_SYNCHRONOUS_COMMIT")
            .ok()
            .is_some_and(|s| matches!(s.trim().to_lowercase().as_str(), "off" | "false" | "0"))
    })
}

/// v4.41 v3 record type tags. Reserve a byte rather than a bit so
/// future record kinds (binary INSERT, multi-row batch, snapshot
/// marker) can all share the v3 frame without another sentinel.
pub(crate) const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;
/// v5.4.0 — durability checkpoint marker. Payload is `[u64 LE
/// byte_offset]`, the WAL byte position where this marker frame
/// starts (i.e. how many bytes of WAL preceded it). Semantics:
/// "every WAL byte before this marker had successfully reached
/// `fsync` at the time the marker was written." The flusher
/// thread in async-commit mode (v5.4.1+) emits one every N
/// records or N microseconds. Replay treats this as a no-op
/// (engine state isn't mutated); the marker is purely metadata
/// for crash-recovery debugging and chaos tests that need to
/// know how much of an async-commit window was durable on kill.
pub(crate) const WAL_V3_TYPE_DURABILITY_CHECKPOINT: u8 = 0x02;

fn encode_wal_record(sql: &str) -> std::io::Result<Vec<u8>> {
    let len = u32::try_from(sql.len())
        .map_err(|_| std::io::Error::other("SQL too large for WAL entry"))?;
    if len & WAL_V2_SENTINEL != 0 {
        return Err(std::io::Error::other(
            "SQL byte count would alias the v4.37 WAL framing sentinel (≥ 2 GiB)",
        ));
    }
    let crc = spg_crypto::crc32::crc32(sql.as_bytes());
    let mut entry = Vec::with_capacity(8 + sql.len());
    entry.extend_from_slice(&(len | WAL_V2_SENTINEL).to_le_bytes());
    entry.extend_from_slice(&crc.to_le_bytes());
    entry.extend_from_slice(sql.as_bytes());
    Ok(entry)
}

/// v4.41 v3 encoder. `payload` is the body bytes (semantics
/// depend on `type_tag`); the returned slice is the framed record
/// `[sentinel|len][crc32(type||payload)][type][payload]`. The CRC
/// covers `type` so a corrupted type byte fails the replay check.
fn encode_wal_v3_record(type_tag: u8, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::other("WAL v3 payload too large"))?;
    // bit 30 + bit 31 are reserved; payload < 1 GiB in practice
    // covers any auto-commit SQL or per-INSERT binary batch we ship.
    if len & (WAL_V2_SENTINEL | WAL_V3_FLAG) != 0 {
        return Err(std::io::Error::other(
            "WAL v3 payload size would alias the v4.41 sentinel bits (≥ 1 GiB)",
        ));
    }
    let mut crc_input = Vec::with_capacity(1 + payload.len());
    crc_input.push(type_tag);
    crc_input.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_input);
    let mut entry = Vec::with_capacity(9 + payload.len());
    entry.extend_from_slice(&(len | WAL_V3_SENTINEL).to_le_bytes());
    entry.extend_from_slice(&crc.to_le_bytes());
    entry.push(type_tag);
    entry.extend_from_slice(payload);
    Ok(entry)
}

/// v4.41 single-record byte total for the v3 auto-commit wrap.
/// 9 bytes of header (4 sentinel+len + 4 CRC + 1 type) plus the
/// SQL payload. Replaces the v4.34 three-v2-record block
/// (`8+5 BEGIN + 8+sql + 8+6 COMMIT` = 35 + sql bytes) with
/// 9 + sql bytes — same quota check, smaller footprint.
fn wal_v3_auto_commit_size(sql: &str) -> u64 {
    9u64 + sql.len() as u64
}

/// v5.4.0 — encode a `durability_checkpoint` v3 record. Payload
/// is the 8-byte LE WAL byte offset where this marker frame
/// starts (i.e. the WAL file length *before* this marker is
/// appended). The framed wrap is the standard v3 envelope:
///
///   `[u32 (len=8 | 0xC000_0000)] [u32 crc32(type || payload)] [type=0x02] [u64 LE byte_offset]`
///
/// Total frame size = 17 bytes. CRC covers `[type || payload]`,
/// matching every other v3 frame.
fn encode_durability_marker(byte_offset: u64) -> std::io::Result<Vec<u8>> {
    encode_wal_v3_record(
        WAL_V3_TYPE_DURABILITY_CHECKPOINT,
        &byte_offset.to_le_bytes(),
    )
}

/// v5.4.0 — append one `durability_checkpoint` marker to the WAL
/// and `sync_data` so the marker plus every byte preceding it is
/// confirmed durable. Returns the WAL byte offset where the marker
/// frame started (= recorded `byte_offset` payload), so callers
/// (the flusher thread in v5.4.1+) can update durability-lag
/// metrics by diffing against the WAL's current end-of-file.
///
/// Shares the same quota / `wal_min_free_bytes` water-mark check
/// the auto-commit write path (`append_wal_v3_group`) runs — a
/// marker that violates the disk-full chaos contract fails the
/// same way an INSERT would, so the flusher thread can degrade
/// gracefully. No-WAL servers return `Ok(0)` (nothing to mark).
///
/// v5.4.4 — lock-free fsync. The marker bytes are written under
/// the `wal` mutex (microseconds), then the mutex is released
/// **before** `sync_data` is called via `wal_sync_clone` (a
/// `try_clone`'d handle to the same underlying file). The OS sees
/// both descriptors as the same file; `sync_data` works on the
/// file's data without needing exclusive access. This decouples
/// the flusher's fsync latency (~5 ms on macOS APFS) from the
/// client write path, restoring the v5.4.2 async-commit throughput
/// promise — without this fix the flusher mutex monopolises the
/// WAL and client INSERTs back up behind fsync (real bug observed
/// in the v5.4.4 smoke test: async mode 9× SLOWER than sync).
fn append_durability_marker(state: &ServerState) -> std::io::Result<u64> {
    let Some(wal_mutex) = state.wal.as_ref() else {
        return Ok(0);
    };
    let pre_marker_offset = {
        let mut wal = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
        let pre_marker_offset = wal.metadata()?.len();
        let entry = encode_durability_marker(pre_marker_offset)?;
        if let Some(quota) = state.chaos.wal_quota_bytes
            && pre_marker_offset.saturating_add(entry.len() as u64) > quota
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded by durability marker: cur={pre_marker_offset} + {} > quota={quota}",
                    entry.len()
                ),
            ));
        }
        if let Some(min_free) = state.limits.wal_min_free_bytes
            && let Some(wal_path) = state.wal_path.as_deref()
        {
            let free = wal_volume_free_bytes(wal_path)?;
            if free < min_free {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "WAL volume below water-mark for durability marker: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                    ),
                ));
            }
        }
        wal.write_all(&entry)?;
        pre_marker_offset
        // wal mutex guard dropped here
    };
    // Fsync without holding the wal mutex. Both `wal_sync_clone`
    // and `wal` reference the same kernel file; `sync_data` only
    // needs `&File`. Client INSERTs can re-acquire the mutex
    // freely during the fsync.
    if let Some(sync_handle) = state.wal_sync_clone.as_ref() {
        sync_handle.sync_data()?;
    } else {
        // Fallback: `try_clone` failed at startup (very rare). Take
        // the mutex briefly to sync — the slow case, but at least
        // correct.
        let wal = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
        wal.sync_data()?;
    }
    Ok(pre_marker_offset)
}

/// v4.42 — concatenate already-framed v3 records for a group of
/// auto-commit writes and append them in **one** `write_all` +
/// **one** `sync_data`. The leader calls this between the prepare
/// and install phases of `run_leader_commit_round` so all writers
/// in the group share a single fsync. `entries` is the framed
/// payload sequence (each item is what `encode_wal_v3_record`
/// produced for one task's SQL). Quota / disk-water-mark checks
/// happen once for the whole batch, so a leader either commits
/// the whole group or rolls back every member — same fan-out
/// invariant `chaos_disk_full_multi_client_group_rollback_all_writers`
/// pins.
fn append_wal_v3_group(state: &ServerState, entries: &[Vec<u8>]) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    if entries.is_empty() {
        return Ok(());
    }
    let total: usize = entries.iter().map(Vec::len).sum();
    let mut batched = Vec::with_capacity(total);
    for e in entries {
        batched.extend_from_slice(e);
    }
    let mut f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    if let Some(quota) = state.chaos.wal_quota_bytes {
        let current = f.metadata().map_or(0, |m| m.len());
        if current.saturating_add(batched.len() as u64) > quota {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded: cur={current} + {} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)",
                    batched.len()
                ),
            ));
        }
    }
    if let Some(min_free) = state.limits.wal_min_free_bytes
        && let Some(wal_path) = state.wal_path.as_deref()
    {
        let free = wal_volume_free_bytes(wal_path)?;
        if free < min_free {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "WAL volume below water-mark: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                ),
            ));
        }
    }
    f.write_all(&batched)?;
    // v5.4.2 — in async-commit mode the flusher thread is
    // responsible for `sync_data`; the client's CC may return
    // before the bytes reach disk. v4.42 group-commit semantics
    // are preserved exactly in sync mode (the default).
    if !synchronous_commit_disabled() {
        f.sync_data()?;
    }
    Ok(())
}

/// v4.42 — `io::Error` is intentionally not `Clone` (the OS error
/// inside it isn't reproducible by value-copy alone), but the
/// commit-barrier leader has to fan one fsync outcome out to N
/// `CommitTask::ack` channels. Reconstruct with the same
/// `ErrorKind` and the original `Display` representation. The
/// chaos test asserts the *kind* is `StorageFull` (so quota-
/// exceeded fan-out is recognisable as ENOSPC by every writer),
/// which this preserves.
fn clone_io_err(e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), e.to_string())
}

/// v4.42 — read `SPG_COMMIT_GROUP_MAX` at queue-pull time so the
/// bench knob can change between connections without a restart.
/// Unset / unparseable / zero → fall back to
/// `DEFAULT_COMMIT_GROUP_MAX`.
fn commit_group_max() -> usize {
    parse_env_usize("SPG_COMMIT_GROUP_MAX").unwrap_or(DEFAULT_COMMIT_GROUP_MAX)
}

/// v4.42 — micro-spin window the leader gives concurrent writers
/// to populate the queue before forming a group. Read fresh from
/// the env on every leader iteration so a benchmark can flip it
/// without a server restart. Mirrors PG's `commit_delay`: zero
/// means "ship what's already queued" (the honest single-client
/// default — group of 1 always, no latency tax), positive N means
/// "spin-wait up to N µs for the queue to fill toward
/// `SPG_COMMIT_GROUP_MAX`". The sweep + multi-client SLO smoke
/// set this to ~200 µs because on macOS APFS a single fsync is
/// ~milliseconds — a 200 µs spin is well under that cost and
/// pays for itself by letting 4-16 writers share one fsync.
fn commit_delay_us() -> u64 {
    parse_env_u64("SPG_COMMIT_DELAY_US").unwrap_or(0)
}

/// v4.42 — push a `CommitTask` onto the commit-barrier queue and
/// decide whether the caller becomes the leader. Returns `true`
/// iff the latching `leader_active` flag flipped from `false` to
/// `true` on this push (= caller is now responsible for driving
/// `run_leader_commit_round`). Returns `false` if another writer
/// is already leading; the caller then waits on its `ack` channel
/// for that leader to commit (or roll back) its task.
fn enqueue_commit_task(state: &ServerState, task: CommitTask) -> bool {
    let mut q = state
        .commit_queue
        .lock()
        .expect("commit queue mutex poisoned");
    q.pending.push_back(task);
    if q.leader_active {
        false
    } else {
        q.leader_active = true;
        true
    }
}

/// v4.42 — leader loop. Runs while `leader_active` is true and
/// pulls one *group* per iteration (up to `commit_group_max()`
/// tasks). Each iteration runs the classic group commit shape,
/// but **sequentially** under one engine write lock so per-task
/// mutations accumulate into shared catalog state (the previous
/// two-phase prepare/install design lost rows: each task's BEGIN
/// cloned the *same* pre-group catalog into its slot, so when
/// COMMIT moved each slot's catalog over `self.catalog` only the
/// last task's slot survived).
///
/// 1. **Snapshot pre-image** — `engine.catalog().clone()`. After
///    the v4.39/v4.40 persistent migration this is an O(1)
///    Arc bump, so the pre-image carries no per-row cost.
///
/// 2. **Sequential prepare + in-memory commit** — for each task:
///    `alloc_tx_id` → `BEGIN` → `execute_in(sql)` → encode v3
///    WAL bytes → `COMMIT` (merges the slot's catalog over
///    `self.catalog`, so the next task's BEGIN sees this task's
///    row). Tasks that fail any step are `ROLLBACK`-ed in
///    isolation and acked with their own error; surviving tasks
///    collect into a `prepared` list keyed by `wal_bytes`.
///    Engine lock released.
///
/// 3. **Batched fsync barrier** — concat survivors' framed v3
///    bytes; one `write_all` + one `sync_data` under the WAL
///    mutex (`append_wal_v3_group`). Quota / disk-water-mark
///    checks happen once for the whole batch — if the batch
///    doesn't fit, every survivor in the group is rolled back
///    together (the multi-client ENOSPC fan-out invariant
///    `chaos_disk_full_multi_client_group_rollback_all_writers`
///    pins).
///
/// 4. **Fsync-fail rollback** — if fsync returned `Err`,
///    re-acquire `engine.write()` and `replace_catalog(pre_image)`
///    to undo every in-memory commit from step 2 at once. Ack
///    each survivor with `{ Ok(exec_result), Err(wal_outcome) }`
///    so dispatch's WAL-error short-circuit reports the failure
///    to the client (and the in-memory state matches the durable
///    state — no phantom rows survive).
///
/// 5. **Ack survivors** — every prepared task is acked here
///    whether fsync succeeded or failed; the dispatch thread's
///    `recv` is the durability contract.
///
/// Rolling drain: after step 5 (or whenever the queue is empty),
/// re-check `state.commit_queue.pending` under the mutex; if
/// new tasks arrived during fsync, loop and form the next group;
/// if not, flip `leader_active = false` and return.
///
/// The function naturally runs >100 lines because group commit's
/// five stages (drain → prepare/in-memory-commit → batched fsync
/// → rollback-on-fail → ack) all touch shared state under the
/// same engine write lock and the same loop iteration; splitting
/// them into helpers would only scatter the control flow.
#[allow(clippy::too_many_lines)]
fn run_leader_commit_round(state: &ServerState) {
    // Per-task scratch carried through the leader's pipeline:
    // declared at module-scope shape so clippy doesn't trip on
    // items-after-statements inside the loop body.
    struct Prepared {
        task: CommitTask,
        result: QueryResult,
        wal_bytes: Vec<u8>,
    }
    let group_max = commit_group_max();
    let delay_us = commit_delay_us();
    loop {
        // ----- 1. Pull one group under the queue lock -----
        //
        // First check non-blocking. If pending is already full or
        // delay_us = 0 (honest single-client default), batch what's
        // there and run the group immediately — group of 1 in the
        // common single-client case, exactly matches the v4.41.1
        // latency shape with no extra wait.
        //
        // If pending is short and delay_us > 0, spin-yield up to
        // `delay_us` microseconds for concurrent writers to push
        // more tasks. Spinning (not sleeping) keeps the wakeup
        // latency sub-microsecond — critical on macOS APFS where
        // a single fsync is multiple milliseconds: a 200 µs spin
        // is cheap insurance to coalesce 4-16 writers into one
        // fsync.
        let group: Vec<CommitTask> = {
            let mut q = state
                .commit_queue
                .lock()
                .expect("commit queue mutex poisoned");
            if delay_us > 0 && q.pending.len() < group_max {
                let deadline = Instant::now() + Duration::from_micros(delay_us);
                while q.pending.len() < group_max && Instant::now() < deadline {
                    drop(q);
                    thread::yield_now();
                    q = state
                        .commit_queue
                        .lock()
                        .expect("commit queue mutex poisoned");
                }
            }
            if q.pending.is_empty() {
                // No more work: drop the leader baton inside the
                // critical section so the next push can claim it
                // atomically.
                q.leader_active = false;
                return;
            }
            let take = q.pending.len().min(group_max);
            q.pending.drain(..take).collect()
        };

        // ----- 2. Sequential prepare + in-memory commit -----
        // Tracks every task that successfully made it through
        // `BEGIN` + sql + `COMMIT` (mutation already merged into
        // `engine.catalog`). Their WAL bytes are concatenated and
        // batched-fsync'd in step 3.
        let mut prepared: Vec<Prepared> = Vec::with_capacity(group.len());
        let pre_image: Option<spg_storage::Catalog> = {
            let Ok(mut engine) = state.engine.write() else {
                // Engine lock poisoned — fatal, server can't make
                // progress. Drop the group (auto-closes every
                // task's ack channel; dispatch threads see
                // `RecvError` and surface a clean io error to
                // their clients) and release the leader baton so
                // future arrivals don't deadlock waiting for a
                // dead leader.
                drop(group);
                if let Ok(mut q) = state.commit_queue.lock() {
                    q.leader_active = false;
                }
                return;
            };
            // O(1) Arc-bump clone (v4.39/v4.40 persistent
            // backing). Stays cheap regardless of row count.
            let pre = engine.catalog().clone();
            for task in group {
                let tx_id = engine.alloc_tx_id();
                if let Err(e) = engine.execute_in("BEGIN", tx_id) {
                    let _ = task.ack.send(CommitResult {
                        result: Err(e),
                        wal_outcome: Ok(()),
                    });
                    continue;
                }
                let exec_res = engine.execute_in_with_cancel(
                    &task.sql,
                    tx_id,
                    spg_engine::CancelToken::from_flag(&task.cancel_flag),
                );
                let was_command_ok = matches!(exec_res, Ok(QueryResult::CommandOk { .. }));
                if !was_command_ok {
                    // SQL itself failed (parse / type / cancel) —
                    // discard the slot via ROLLBACK in isolation
                    // so other tasks in the group aren't affected,
                    // ack with the engine error.
                    let _ = engine.execute_in("ROLLBACK", tx_id);
                    let _ = task.ack.send(CommitResult {
                        result: exec_res,
                        wal_outcome: Ok(()),
                    });
                    continue;
                }
                // Encode v3 framed bytes. Encoding-time failures
                // (payload > 1 GiB) abort just this task's slot,
                // not the group.
                let wal_bytes =
                    match encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, task.sql.as_bytes()) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = engine.execute_in("ROLLBACK", tx_id);
                            let _ = task.ack.send(CommitResult {
                                result: exec_res,
                                wal_outcome: Err(e),
                            });
                            continue;
                        }
                    };
                // In-memory COMMIT — merges this slot's catalog
                // over `engine.catalog`. The next task's BEGIN
                // (above) clones *this* catalog, so per-task
                // mutations accumulate. If COMMIT itself fails
                // (rare — would mean `NoActiveTransaction`,
                // which it isn't since we just BEGIN'd) ROLLBACK
                // the slot and ack the task with the engine
                // error; carry on with the rest of the group.
                if let Err(e) = engine.execute_in("COMMIT", tx_id) {
                    let _ = engine.execute_in("ROLLBACK", tx_id);
                    let _ = task.ack.send(CommitResult {
                        result: Err(e),
                        wal_outcome: Ok(()),
                    });
                    continue;
                }
                prepared.push(Prepared {
                    task,
                    result: exec_res.unwrap(),
                    wal_bytes,
                });
            }
            // Hand back the pre-image only if we actually
            // mutated state; that's the only case where a fsync
            // failure would need to roll back.
            if prepared.is_empty() { None } else { Some(pre) }
        }; // engine write lock released here

        if prepared.is_empty() {
            // Whole group failed prepare; nothing to fsync, no
            // rollback needed. Loop to pull the next group.
            continue;
        }

        // ----- 3. Batched fsync barrier -----
        let entries: Vec<Vec<u8>> = prepared.iter().map(|p| p.wal_bytes.clone()).collect();
        let wal_outcome: std::io::Result<()> = append_wal_v3_group(state, &entries);

        // ----- 4. Fsync-fail rollback -----
        if wal_outcome.is_err()
            && let Some(pre) = pre_image
        {
            if let Ok(mut engine) = state.engine.write() {
                engine.replace_catalog(pre);
            } else {
                // Poisoned mid-rollback: every survivor's ack
                // channel will surface the WAL error anyway, but
                // the catalog now diverges from the durable WAL.
                // Leader can't fix that; bail and let the next
                // bootup's WAL replay reconverge.
                drop(prepared);
                if let Ok(mut q) = state.commit_queue.lock() {
                    q.leader_active = false;
                }
                return;
            }
        }

        // ----- 5. Ack survivors -----
        // Dispatch checks `wal_outcome` first (the v4.41.1
        // "WAL append failed: ..." error shape lives in that
        // branch), so even when the in-memory exec succeeded but
        // fsync failed, the client sees the WAL error and
        // recovers to a state consistent with the durable WAL.
        for p in prepared {
            let cloned_wal = match &wal_outcome {
                Ok(()) => Ok(()),
                Err(e) => Err(clone_io_err(e)),
            };
            let _ = p.task.ack.send(CommitResult {
                result: Ok(p.result),
                wal_outcome: cloned_wal,
            });
        }
        // loop back to pull the next group (rolling drain).
    }
}

fn append_wal(state: &ServerState, sql: &str) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    let entry = encode_wal_record(sql)?;
    let mut f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    // v4.29 chaos: simulated disk-full. Reject the append before
    // touching the OS so committed state is unaffected. Returned
    // error propagates as a clean ErrorResponse to the client.
    if let Some(quota) = state.chaos.wal_quota_bytes {
        let current = f.metadata().map_or(0, |m| m.len());
        if current.saturating_add(entry.len() as u64) > quota {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded: cur={current} + {} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)",
                    entry.len()
                ),
            ));
        }
    }
    // v4.33 disk water-mark: when `SPG_WAL_MIN_FREE_BYTES` is set,
    // call statvfs on the WAL volume and refuse the append if free
    // space is below the threshold. Writes return StorageFull; reads
    // continue (this path is write-only). Defaults off — when unset,
    // `state.limits.wal_min_free_bytes` is None and we skip the
    // syscall entirely.
    if let Some(min_free) = state.limits.wal_min_free_bytes
        && let Some(wal_path) = state.wal_path.as_deref()
    {
        let free = wal_volume_free_bytes(wal_path)?;
        if free < min_free {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "WAL volume below water-mark: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                ),
            ));
        }
    }
    f.write_all(&entry)?;
    // v5.4.2 — async-commit mode opts out of the per-write
    // `sync_data`; durability rides on the flusher thread's
    // periodic `durability_checkpoint` markers instead.
    if !synchronous_commit_disabled() {
        f.sync_data()?;
    }
    Ok(())
}

/// v4.33: free bytes on the filesystem that owns `path`, via
/// `statvfs(2)`. macOS and Linux both expose `statvfs` with
/// compatible field semantics (`f_bavail` × `f_frsize`).
/// `f_bavail` (vs `f_bfree`) excludes blocks reserved for the
/// superuser, which is what an unprivileged write actually has
/// access to — the same number `df` shows in its "Avail" column.
///
/// The `as u64` casts are widening on every supported platform
/// (`fsblkcnt_t`/`c_ulong` are u32 on apple, u64 on linux); pin
/// the lossless-cast lint locally so the same source compiles
/// cleanly on both without per-cfg branches.
#[allow(unsafe_code, clippy::cast_lossless, clippy::useless_conversion)]
fn wal_volume_free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut c_path = Vec::with_capacity(bytes.len() + 1);
    c_path.extend_from_slice(bytes);
    c_path.push(0);
    // SAFETY: `statvfs` reads a NUL-terminated path and writes into
    // the provided buffer. We give it both. The buffer is initialized
    // by the call (the kernel writes every field on success); on
    // failure we return early via the errno check before reading any
    // field. macOS + Linux `libc::statvfs` signatures match.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr().cast(), &raw mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bavail = stat.f_bavail as u64;
    let frsize = stat.f_frsize as u64;
    Ok(bavail.saturating_mul(frsize))
}

/// Replay WAL bytes onto `engine`. Returns the number of entries applied.
/// Handles all three record formats:
///   v1 (≤ v4.36): `[u32 len][len bytes]` — no CRC. bit 31 = 0.
///   v2 (v4.37+):  `[u32 (len | 0x8000_0000)][u32 crc32][len bytes]`.
///                 bit 31 = 1, bit 30 = 0.
///   v3 (v4.41+):  `[u32 (len | 0xC000_0000)][u32 crc32][1 byte type][len bytes payload]`.
///                 bit 31 = 1, bit 30 = 1. The CRC covers
///                 `[type byte || payload]`. Unknown type byte is
///                 fatal — never silently skipped.
/// The format is detected per-record by the sentinel bits; a WAL
/// file that interleaves multiple versions (mid-upgrade) still
/// replays correctly. A truncated trailing entry (e.g. crash mid-
/// append) is dropped with a warning to stderr. Non-truncation
/// errors — engine rejected SQL, bad UTF-8, CRC mismatch, unknown
/// v3 type — are fatal: the operator must inspect.
///
/// v5.4: type-tag dispatch is delegated to `dispatch_v3_record` so
/// new v3 kinds (like `durability_checkpoint`) extend the namespace
/// without inflating this function past the per-function line
/// budget.
fn dispatch_v3_record(
    tag: u8,
    payload: &[u8],
    frame_off: usize,
    engine: &mut Engine,
) -> std::io::Result<bool> {
    match tag {
        WAL_V3_TYPE_AUTO_COMMIT_SQL => {
            let sql = core::str::from_utf8(payload).map_err(|_| {
                std::io::Error::other("v3 auto_commit_sql payload has non-UTF-8 SQL")
            })?;
            engine
                .execute(sql)
                .map_err(|e| std::io::Error::other(format!("WAL replay rejected {sql:?}: {e}")))?;
            Ok(true)
        }
        WAL_V3_TYPE_DURABILITY_CHECKPOINT => {
            // v5.4.0 — marker is a no-op during replay (engine state
            // isn't mutated); its purpose is to record "every WAL byte
            // before this marker was fsynced by the flusher at write
            // time." Validate payload shape + cross-check the recorded
            // offset against `frame_off`; a mismatch logs a stderr
            // warning (would indicate WAL relocation) but replay keeps
            // going. `Ok(false)` opts the marker out of the user-SQL
            // applied counter.
            if payload.len() != 8 {
                return Err(std::io::Error::other(format!(
                    "WAL durability_checkpoint at offset {frame_off} has {}-byte payload (expected 8)",
                    payload.len()
                )));
            }
            let arr: [u8; 8] = payload.try_into().expect("checked len above");
            let recorded_off = u64::from_le_bytes(arr);
            let frame_off_u64 = frame_off as u64;
            if recorded_off != frame_off_u64 {
                eprintln!(
                    "spg-server: WAL durability_checkpoint at offset {frame_off} carries recorded_off={recorded_off} — possible WAL relocation; treating marker as no-op"
                );
            }
            Ok(false)
        }
        other => Err(std::io::Error::other(format!(
            "WAL v3 unknown type byte {other:#04x} at offset {frame_off} — refusing to replay"
        ))),
    }
}

fn replay_wal_bytes(bytes: &[u8], engine: &mut Engine) -> std::io::Result<usize> {
    let mut cur = 0;
    let mut applied = 0usize;
    while cur < bytes.len() {
        if bytes.len() - cur < 4 {
            eprintln!(
                "spg-server: WAL truncated at offset {cur} (need 4-byte length, have {})",
                bytes.len() - cur
            );
            break;
        }
        let frame_off = cur;
        let len_arr: [u8; 4] = bytes[cur..cur + 4].try_into().expect("checked");
        let raw_len = u32::from_le_bytes(len_arr);
        cur += 4;
        let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
        let is_v3 = is_v2 && (raw_len & WAL_V3_FLAG != 0);
        // v3 reuses the v2 sentinel bit + adds bit 30; mask both
        // when extracting the length so v3 lengths read correctly.
        let len_mask = if is_v3 {
            !(WAL_V2_SENTINEL | WAL_V3_FLAG)
        } else {
            !WAL_V2_SENTINEL
        };
        let len = (raw_len & len_mask) as usize;
        let expected_crc = if is_v2 {
            if bytes.len() - cur < 4 {
                eprintln!(
                    "spg-server: v2/v3 WAL truncated at offset {cur} (need 4-byte CRC, have {})",
                    bytes.len() - cur
                );
                break;
            }
            let crc_arr: [u8; 4] = bytes[cur..cur + 4].try_into().expect("checked");
            cur += 4;
            Some(u32::from_le_bytes(crc_arr))
        } else {
            None
        };
        // v3 carries a 1-byte type tag between the CRC and the
        // payload body. Read it here so the rest of the loop sees
        // a uniform `payload` slice.
        let v3_type_tag = if is_v3 {
            if bytes.len() - cur < 1 {
                eprintln!(
                    "spg-server: v3 WAL truncated at offset {cur} (need 1-byte type, have 0)"
                );
                break;
            }
            let t = bytes[cur];
            cur += 1;
            Some(t)
        } else {
            None
        };
        if cur + len > bytes.len() {
            eprintln!("spg-server: WAL entry truncated (payload_len={len}) — dropping tail");
            break;
        }
        let payload = &bytes[cur..cur + len];
        if let Some(expected) = expected_crc {
            let actual = if let Some(tag) = v3_type_tag {
                // CRC covers `[type byte || payload]` in v3 so a
                // flipped type byte fails the check.
                let mut buf = Vec::with_capacity(1 + payload.len());
                buf.push(tag);
                buf.extend_from_slice(payload);
                spg_crypto::crc32::crc32(&buf)
            } else {
                spg_crypto::crc32::crc32(payload)
            };
            if actual != expected {
                return Err(std::io::Error::other(format!(
                    "WAL CRC mismatch at offset {frame_off} (expected={expected:#010x}, computed={actual:#010x}, payload_len={len}) — corruption detected, refusing to replay"
                )));
            }
        }
        // Dispatch by frame version. v1/v2 payload is the SQL text
        // directly; v3 routes on the type tag via `dispatch_v3_record`,
        // which returns `false` only for metadata records (v5.4
        // `durability_checkpoint`) that shouldn't increment the user-
        // SQL `applied` counter.
        let count_as_applied = if let Some(tag) = v3_type_tag {
            dispatch_v3_record(tag, payload, frame_off, engine)?
        } else {
            let sql = core::str::from_utf8(payload)
                .map_err(|_| std::io::Error::other("WAL entry has non-UTF-8 SQL"))?;
            engine
                .execute(sql)
                .map_err(|e| std::io::Error::other(format!("WAL replay rejected {sql:?}: {e}")))?;
            true
        };
        cur += len;
        if count_as_applied {
            applied += 1;
        }
    }
    Ok(applied)
}

/// v6.5.3 — public alias so the pgwire crate can append audit
/// entries on catalog-mutating statements.
pub(crate) fn append_audit_pub(state: &ServerState, sql: &str) -> std::io::Result<()> {
    append_audit(state, sql)
}

fn append_audit(state: &ServerState, sql: &str) -> std::io::Result<()> {
    let ts_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(u64::MAX);
    let mut log = state
        .audit_log
        .lock()
        .map_err(|_| std::io::Error::other("audit mutex poisoned"))?;
    log.append(sql.to_string(), ts_ms);
    if let Some(path) = state.audit_path.as_deref() {
        let mut entry_bytes = Vec::new();
        log.encode_entry_to(log.len() - 1, &mut entry_bytes);
        let mut f = OpenOptions::new().append(true).open(path)?;
        f.write_all(&entry_bytes)?;
    }
    Ok(())
}

/// Write `data` to `path` atomically: write to a sibling tmp file then
/// `rename` over the target. `rename` is atomic on POSIX.
/// Wall clock impl injected into the engine. Microseconds since the
/// Unix epoch; clamps to `i64::MAX` for far-future system clocks.
pub(crate) fn wall_clock_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// v4.1: when the engine has no users yet and `SPG_ADMIN_PASSWORD`
/// is set in the environment, create an admin so a fresh
/// docker-compose deployment has someone who can manage further
/// users. Once an admin exists in the snapshot, changing the env
/// var has no effect on restart (use SQL to rotate passwords).
/// Default username is "admin"; override with `SPG_ADMIN_USER`.
fn bootstrap_admin_from_env(engine: &mut Engine, db_path: Option<&Path>) -> std::io::Result<()> {
    if !engine.users().is_empty() {
        return Ok(());
    }
    let Ok(pw) = env::var("SPG_ADMIN_PASSWORD") else {
        return Ok(());
    };
    if pw.is_empty() {
        return Ok(());
    }
    let user = env::var("SPG_ADMIN_USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "admin".to_string());
    let salt = random_salt()?;
    engine
        .create_user(&user, &pw, Role::Admin, salt)
        .map_err(|e| std::io::Error::other(format!("bootstrap admin {user:?}: {e}")))?;
    eprintln!("spg-server: bootstrapped admin user {user:?} from SPG_ADMIN_PASSWORD");
    // Persist immediately so the bootstrap survives without waiting
    // for the first successful DDL/DML to trigger a snapshot.
    if let Some(p) = db_path {
        let snapshot = engine.snapshot();
        if let Err(e) = write_atomic(p, &snapshot) {
            eprintln!("spg-server: warning — failed to persist bootstrap admin: {e}");
        } else {
            // v5.3.1 — sidecar manifest. Bootstrap-time call has
            // an empty cold-segment registry and wal_baseline_offset
            // = current WAL length (0 for a fresh deploy).
            // Best-effort.
            write_manifest_alongside(p, &snapshot, &BTreeMap::new(), 0);
        }
    }
    Ok(())
}

/// v4.33 slow-query log scope. Records the dispatch start `Instant`
/// at construction and, on `Drop`, emits one JSON line to stderr if
/// elapsed exceeds the configured `SPG_SLOW_QUERY_LOG_MS` threshold.
/// Threshold = `None` makes Drop a no-op (zero-cost when the env var
/// isn't set). Field naming intentionally matches the existing
/// `SPG_LOG_FORMAT=json` pipeline so both event streams ingest the
/// same way.
struct SlowLogGuard<'a> {
    threshold_us: Option<u64>,
    sql: &'a str,
    role: Option<Role>,
    start: Instant,
}

impl<'a> SlowLogGuard<'a> {
    fn new(state: &ServerState, sql: &'a str, role: Option<Role>) -> Self {
        Self {
            threshold_us: state
                .limits
                .slow_query_log_ms
                .map(|ms| ms.saturating_mul(1000)),
            sql,
            role,
            start: Instant::now(),
        }
    }
}

impl Drop for SlowLogGuard<'_> {
    fn drop(&mut self) {
        let Some(threshold_us) = self.threshold_us else {
            return;
        };
        let elapsed_us = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
        if elapsed_us < threshold_us {
            return;
        }
        let mut sql_escaped = String::with_capacity(self.sql.len() + 16);
        json_escape_into(self.sql, &mut sql_escaped);
        let role_str = self.role.map_or("unauth", Role::as_str);
        eprintln!(
            r#"{{"event":"slow_query","sql":"{sql_escaped}","elapsed_us":{elapsed_us},"role":"{role_str}","threshold_us":{threshold_us}}}"#
        );
    }
}

/// Minimal JSON string escaper for the slow-query log line. Handles
/// the seven escapes JSON requires (\\, \", \b, \f, \n, \r, \t) and
/// emits `\u00XX` for the remaining control bytes. UTF-8 sequences
/// pass through verbatim — JSON strings allow raw multibyte UTF-8.
fn json_escape_into(s: &str, out: &mut String) {
    use std::fmt::Write as _;
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// v4.5: per-query watchdog. Reads `state.limits.query_timeout_ms`;
/// when set, spawns a thread that sleeps the budget then flips
/// `cancel_flag`. `Watchdog::cancel` is idempotent — call it once
/// the query completes so the watchdog thread sees no work and the
/// next query gets a fresh budget.
struct Watchdog {
    /// Shared with both this struct and the watchdog thread. Setting
    /// it stops the timer's sleep loop without needing to join.
    completed: Arc<AtomicBool>,
}

impl Watchdog {
    fn cancel(&self) {
        self.completed.store(true, Ordering::Release);
    }
}

fn spawn_query_watchdog(state: &ServerState, cancel_flag: &Arc<AtomicBool>) -> Watchdog {
    let completed = Arc::new(AtomicBool::new(false));
    let Some(budget_ms) = state.limits.query_timeout_ms else {
        return Watchdog { completed };
    };
    let cancel_flag = Arc::clone(cancel_flag);
    let completed_for_thread = Arc::clone(&completed);
    thread::spawn(move || {
        // Sleep in short slices so a finished query reclaims the
        // watchdog quickly (avoids piling up parked threads when
        // SPG_QUERY_TIMEOUT_MS is high but queries are usually fast).
        let total = std::time::Duration::from_millis(budget_ms);
        let slice = std::time::Duration::from_millis(50.min(budget_ms));
        let start = std::time::Instant::now();
        while start.elapsed() < total {
            if completed_for_thread.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(slice);
        }
        cancel_flag.store(true, Ordering::Release);
    });
    Watchdog { completed }
}

/// 16 cryptographically random bytes from the OS via /dev/urandom.
/// Used as per-user salt for password hashing. Falls back to error
/// if /dev/urandom is unreadable — better fail loudly at startup
/// than silently degrade salt randomness.
fn random_salt() -> std::io::Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// Salt source for the engine. Engine's `SaltFn` is infallible; if
/// /dev/urandom is unreadable we panic — running without a working
/// kernel RNG is an environmental failure, not a recoverable
/// condition. spg-server's startup already eats the same panic
/// surface for any other OS-level resource gap.
fn urandom_salt_or_panic() -> [u8; 16] {
    random_salt().expect("/dev/urandom unreadable — refusing to create users without entropy")
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let pid = process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!(".spg-tmp-{pid}-{nanos}"));
    fs::write(&tmp, data)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// v5.3.1 — write a `CatalogManifest` for `db_path` alongside the
/// snapshot. Called after every successful `write_atomic` of a
/// snapshot. Best-effort: a manifest write failure surfaces as a
/// stderr log but does NOT fail the snapshot — the WAL is still the
/// primary durability surface in v5.3.x, and a missing manifest
/// only loses the boot-time optimisation (legacy
/// `SPG_PRELOAD_COLD_SEGMENT` keeps working).
///
/// `wal_baseline_offset` is the byte offset in the WAL where future
/// replay should start. In v5.3.1 every snapshot write captures the
/// current WAL file length; v5.3.2 wires this into the replay-skip
/// path so 100M boot stays under 60 s.
fn write_manifest_alongside(
    db_path: &Path,
    snapshot_bytes: &[u8],
    cold_segment_paths: &BTreeMap<u32, PathBuf>,
    wal_baseline_offset: u64,
) {
    let mp = manifest::manifest_path(db_path);
    if let Some(dir) = mp.parent()
        && let Err(e) = fs::create_dir_all(dir)
    {
        eprintln!(
            "spg-server: manifest dir {} mkdir failed: {e}",
            dir.display()
        );
        return;
    }
    let cold_segments: Vec<manifest::ColdSegmentEntry> = cold_segment_paths
        .iter()
        .filter_map(|(&segment_id, path)| match fs::read(path) {
            Ok(bytes) => Some(manifest::ColdSegmentEntry {
                segment_id,
                path: path.clone(),
                crc32: spg_crypto::crc32::crc32(&bytes),
            }),
            Err(e) => {
                eprintln!(
                    "spg-server: manifest skip segment {segment_id}: read {} failed: {e}",
                    path.display()
                );
                None
            }
        })
        .collect();
    let m = manifest::CatalogManifest {
        catalog_crc32: spg_crypto::crc32::crc32(snapshot_bytes),
        cold_segments,
        wal_baseline_offset,
    };
    let bytes = m.serialize();
    if let Err(e) = write_atomic(&mp, &bytes) {
        eprintln!("spg-server: manifest write to {} failed: {e}", mp.display());
    }
}

/// v5.3.1 — boot-side manifest read. Called after the snapshot has
/// been restored into `engine` but before WAL replay. If the
/// manifest is present and its `catalog_crc32` matches a fresh
/// CRC32 over `snapshot_bytes`, every recorded cold segment is
/// loaded into the engine catalog and the `segment_id` → path map is
/// populated on the in-flight `cold_segment_paths`. Returns the
/// `wal_baseline_offset` the WAL replay should start from (or 0
/// when no usable manifest exists). Mismatches and parse errors
/// surface as stderr warnings; in every error path the legacy
/// "no manifest, replay from 0" behaviour wins.
fn load_manifest_and_preload_cold(
    engine: &mut Engine,
    db_path: &Path,
    snapshot_bytes: &[u8],
    cold_segment_paths: &mut BTreeMap<u32, PathBuf>,
) -> u64 {
    let mp = manifest::manifest_path(db_path);
    if !mp.exists() {
        return 0;
    }
    let bytes = match fs::read(&mp) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("spg-server: manifest read {} failed: {e}", mp.display());
            return 0;
        }
    };
    let m = match manifest::CatalogManifest::deserialize(&bytes) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("spg-server: manifest {} rejected: {e}", mp.display());
            return 0;
        }
    };
    let snapshot_crc = spg_crypto::crc32::crc32(snapshot_bytes);
    if snapshot_crc != m.catalog_crc32 {
        eprintln!(
            "spg-server: manifest {} catalog CRC mismatch (expected={:#010x}, file={:#010x}); \
             falling back to WAL-only replay",
            mp.display(),
            m.catalog_crc32,
            snapshot_crc,
        );
        return 0;
    }
    let mut cat = engine.catalog().clone();
    let mut loaded: usize = 0;
    let mut skipped: usize = 0;
    for entry in &m.cold_segments {
        match fs::read(&entry.path) {
            Ok(seg_bytes) => {
                let computed = spg_crypto::crc32::crc32(&seg_bytes);
                if computed != entry.crc32 {
                    eprintln!(
                        "spg-server: manifest skip segment {}: CRC mismatch ({} != {})",
                        entry.segment_id, computed, entry.crc32
                    );
                    skipped += 1;
                    continue;
                }
                match cat.load_segment_bytes(seg_bytes) {
                    Ok(new_id) => {
                        cold_segment_paths.insert(new_id, entry.path.clone());
                        loaded += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "spg-server: manifest segment {} load failed: {e}",
                            entry.segment_id
                        );
                        skipped += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "spg-server: manifest skip segment {}: read {} failed: {e}",
                    entry.segment_id,
                    entry.path.display()
                );
                skipped += 1;
            }
        }
    }
    engine.replace_catalog(cat);
    eprintln!(
        "spg-server: manifest {} loaded {loaded} cold segment(s), skipped {skipped}; wal_baseline_offset={}",
        mp.display(),
        m.wal_baseline_offset,
    );
    m.wal_baseline_offset
}

fn emit_result(
    stream: &mut TcpStream,
    result: Result<QueryResult, EngineError>,
) -> std::io::Result<()> {
    match result {
        Ok(QueryResult::CommandOk { affected, .. }) => {
            write_frame(stream, &build_command_complete(affected as u64))
        }
        Ok(QueryResult::Rows { columns, rows }) => {
            // v3.3.1: encode the entire response (RowDescription +
            // DataRowBatch chunks + CommandComplete) into one Vec<u8>
            // then a single write_all. Saves 2 syscalls per SELECT vs
            // the old 3-write_frame path.
            let descs = columns
                .iter()
                .map(column_schema_to_desc)
                .collect::<Vec<_>>();
            let rd =
                build_row_description(&descs).map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut out: Vec<u8> = Vec::with_capacity(
                spg_wire::FRAME_HEADER_LEN + rd.payload.len() + rows.len() * 64 + 16,
            );
            encode(&rd, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
            if rows.len() <= 1 {
                for row in rows {
                    let wire = row_to_wire(&row);
                    let frame =
                        build_data_row(&wire).map_err(|e| std::io::Error::other(e.to_string()))?;
                    encode(&frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
            } else {
                let wire_rows: Vec<Vec<WireValue>> = rows.iter().map(row_to_wire).collect();
                for chunk in wire_rows.chunks(BATCH_ROWS_PER_FRAME) {
                    let frame = build_data_row_batch(chunk)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    encode(&frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
            }
            let cc = build_command_complete(0);
            encode(&cc, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
            stream.write_all(&out)
        }
        Err(e) => write_frame(stream, &build_error_response(&e.to_string())),
    }
}

fn column_schema_to_desc(c: &ColumnSchema) -> ColumnDesc {
    ColumnDesc {
        name: c.name.clone(),
        ty: data_type_to_wire(c.ty),
        nullable: c.nullable,
    }
}

const fn data_type_to_wire(t: DataType) -> WireType {
    match t {
        // v1.11 surfaces SMALLINT as INT on the wire — the wire layer
        // doesn't (yet) carry a separate 16-bit tag, and PG drivers
        // happily render an i32 for any narrower integer column.
        DataType::SmallInt | DataType::Int => WireType::Int,
        DataType::BigInt => WireType::BigInt,
        DataType::Float => WireType::Float,
        // VARCHAR / CHAR / NUMERIC / DATE / TIMESTAMP collapse to
        // TEXT on the wire. Schema tracks bounds and precision; values
        // are plain UTF-8 in their canonical text forms.
        DataType::Text
        | DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Numeric { .. }
        | DataType::Date
        | DataType::Timestamp
        | DataType::Interval
        | DataType::Json => WireType::Text,
        DataType::Bool => WireType::Bool,
        // RowDescription drops the dimension; DataRow's WireValue::Vector
        // carries the actual element count back to the client.
        DataType::Vector { .. } => WireType::Vector,
    }
}

fn row_to_wire(r: &Row) -> Vec<WireValue> {
    r.values.iter().map(value_to_wire).collect()
}

fn value_to_wire(v: &Value) -> WireValue {
    match v {
        Value::Null => WireValue::Null,
        // SMALLINT widens to wire INT — drivers see a plain i32.
        Value::SmallInt(n) => WireValue::Int(i32::from(*n)),
        Value::Int(n) => WireValue::Int(*n),
        Value::BigInt(n) => WireValue::BigInt(*n),
        Value::Float(x) => WireValue::Float(*x),
        // v4.9: TEXT and JSON ride the wire identically — the
        // client's column type (RowDescription OID) carries the
        // "this is JSON" semantic.
        Value::Text(s) | Value::Json(s) => WireValue::Text(s.clone()),
        Value::Bool(b) => WireValue::Bool(*b),
        Value::Vector(v) => WireValue::Vector(v.clone()),
        // v6.0.1: SQ8 cells dequantise to f32 on the wire so
        // pgwire clients (psql, drivers, the conformance corpora)
        // see the same `WireValue::Vector` shape regardless of
        // the column's storage encoding. Recall envelope absorbs
        // the ≤ (max-min)/255/2 dequantisation error.
        Value::Sq8Vector(q) => WireValue::Vector(spg_storage::quantize::dequantize(q)),
        // v6.0.3: HalfVector cells decode bit-exactly back to f32.
        Value::HalfVector(h) => WireValue::Vector(h.to_f32_vec()),
        // NUMERIC / DATE / TIMESTAMP render as their canonical
        // text form on the wire. Drivers receive plain UTF-8,
        // identical to what `value_to_text` produces in the engine.
        Value::Numeric { scaled, scale } => {
            WireValue::Text(spg_engine::eval::format_numeric(*scaled, *scale))
        }
        Value::Date(d) => WireValue::Text(spg_engine::eval::format_date(*d)),
        Value::Timestamp(t) => WireValue::Text(spg_engine::eval::format_timestamp(*t)),
        Value::Interval { months, micros } => {
            WireValue::Text(spg_engine::eval::format_interval(*months, *micros))
        }
    }
}

fn write_frame(stream: &mut TcpStream, frame: &Frame) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode(frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&out)
}

#[cfg(test)]
mod wal_v3_durability_marker_tests {
    use super::{
        Engine, WAL_V2_SENTINEL, WAL_V3_FLAG, WAL_V3_SENTINEL, WAL_V3_TYPE_AUTO_COMMIT_SQL,
        WAL_V3_TYPE_DURABILITY_CHECKPOINT, encode_durability_marker, encode_wal_v3_record,
        replay_wal_bytes,
    };

    #[test]
    fn durability_marker_frame_shape_pins_v3_wire() {
        // Wire-format pin: a marker for byte_offset=0x1234_5678 must
        // produce the v3 envelope `[sentinel|len=8][crc][type=0x02]
        // [u64 LE offset]` — 17 bytes total. Any future change to
        // the frame layout breaks this test, forcing a STABILITY
        // bump conversation.
        let bytes = encode_durability_marker(0x1234_5678).unwrap();
        assert_eq!(bytes.len(), 17, "marker frame must be 17 bytes");
        let raw_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let len_field = raw_len & !(WAL_V2_SENTINEL | WAL_V3_FLAG);
        assert_eq!(len_field, 8, "marker payload is 8 bytes (the u64 offset)");
        assert_eq!(
            raw_len & WAL_V3_SENTINEL,
            WAL_V3_SENTINEL,
            "marker must carry v3 sentinel bits",
        );
        assert_eq!(
            bytes[8], WAL_V3_TYPE_DURABILITY_CHECKPOINT,
            "type byte must be 0x02",
        );
        let offset = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
        assert_eq!(offset, 0x1234_5678, "payload echoes the offset arg");
    }

    #[test]
    fn replay_skips_durability_markers_and_does_not_increment_applied() {
        // A WAL containing only durability markers replays as a
        // no-op: applied=0, no engine mutation. Three markers at
        // different "recorded offsets" — none match the actual
        // frame_off in this synthetic stream (the first marker is
        // at byte 0, the others follow), so the consistency check
        // hits stderr but replay keeps going.
        let mut stream = Vec::new();
        stream.extend_from_slice(&encode_durability_marker(0).unwrap());
        stream.extend_from_slice(&encode_durability_marker(17).unwrap());
        stream.extend_from_slice(&encode_durability_marker(34).unwrap());
        let mut engine = Engine::new();
        let applied = replay_wal_bytes(&stream, &mut engine).expect("replay must accept markers");
        assert_eq!(applied, 0, "markers do not count as applied records");
    }

    #[test]
    fn replay_mixes_sql_and_markers_advancing_cursor_correctly() {
        // Marker interleaved between two CREATE TABLE statements
        // must not affect cursor accounting: both CREATE TABLEs
        // apply, marker no-ops, applied=2.
        let mut stream = Vec::new();
        let create_a =
            encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, b"CREATE TABLE a (id INT)").unwrap();
        let create_b =
            encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, b"CREATE TABLE b (id INT)").unwrap();
        let marker_off = create_a.len() as u64;
        let marker = encode_durability_marker(marker_off).unwrap();
        stream.extend_from_slice(&create_a);
        stream.extend_from_slice(&marker);
        stream.extend_from_slice(&create_b);
        let mut engine = Engine::new();
        let applied =
            replay_wal_bytes(&stream, &mut engine).expect("mixed stream must replay cleanly");
        assert_eq!(
            applied, 2,
            "two CREATE TABLEs applied; marker doesn't count"
        );
    }

    #[test]
    fn replay_rejects_marker_with_wrong_payload_length() {
        // A v3 frame typed 0x02 but carrying a payload != 8 bytes is
        // a structural error — replay must surface it, not silently
        // tolerate it. Forge such a frame via `encode_wal_v3_record`
        // with a 4-byte payload.
        let bad =
            encode_wal_v3_record(WAL_V3_TYPE_DURABILITY_CHECKPOINT, &0u32.to_le_bytes()).unwrap();
        let mut engine = Engine::new();
        let err = replay_wal_bytes(&bad, &mut engine).expect_err("4-byte payload must error");
        let msg = err.to_string();
        assert!(
            msg.contains("durability_checkpoint") && msg.contains("4-byte payload"),
            "error message should name the malformed marker: got {msg:?}",
        );
    }
}
