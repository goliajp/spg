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
mod prefetch;
mod pubsub;

thread_local! {
    /// v6.7.6 — single-cell handoff for the prefetch hit count.
    /// `load_manifest_and_preload_cold` runs before
    /// `Arc<ServerState>` is constructed, so it stashes the count
    /// here and `main()` drains it into `state.metrics
    /// .cold_prefetch_hits` after the state is built.
    static PREFETCH_HITS_BOOT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
mod commands;
mod mysqlwire;
mod pgwire;
mod replication;
mod scram;
mod wal;
mod wire;

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
    Frame, FrameError, Op, build_error_response, build_stats_response, decode, parse_auth,
    parse_auth_user, parse_query,
};

pub(crate) use commands::*;
pub(crate) use wal::*;
use wire::{emit_result, write_frame};

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
    /// v6.10.1 — `SPG_MAX_QUERY_NS` per-query CPU/wall budget in
    /// nanoseconds. Finer-grained than `query_timeout_ms` (1ms
    /// resolution): a 250µs query budget surfaces as 250000 ns
    /// here, whereas `_MS` rounds to 1 ms. When both envs are
    /// set, the *tighter* effective deadline wins. Defaults to
    /// `None` (no budget, same as the legacy path).
    max_query_ns: Option<u64>,
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
    /// v7.37.15 (r172) — effective `synchronous_commit` captured at
    /// dispatch time (session GUC, falling back to the env default).
    /// The leader fsyncs a group iff any member wants sync; an
    /// all-async group's append skips `sync_data` and the flusher
    /// bounds the loss window.
    sync_commit: bool,
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
    /// v7.37.15 (r172) — latched true the first time a commit skips
    /// its per-write fsync because the session GUC
    /// `synchronous_commit = off` asked for async commit while the
    /// process default is sync. The always-running flusher watches
    /// this flag and starts emitting periodic durability markers +
    /// fsync once set, bounding the async loss window the same way
    /// `SPG_SYNCHRONOUS_COMMIT=off` mode does (PG wal-writer
    /// semantics). Never cleared — like PG's wal writer, once async
    /// commits exist the periodic flush keeps running.
    pub(crate) async_commit_dirty: AtomicBool,
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
    /// v7.17 Phase 2.4 — startup-param `application_name` (or the
    /// last value the client sent via `SET application_name = '...'`).
    /// Session-scoped per PG semantics; SET LOCAL is not honored
    /// because PG itself treats this GUC as session-only.
    pub(crate) application_name: RwLock<String>,
    /// v7.39 (GUC) — the startup-packet `application_name`. RESET /
    /// RESET ALL restore to this (PG's per-session default), not to
    /// the compiled-in empty string.
    pub(crate) startup_app_name: String,
    /// v7.39 (query cancel) — the secret half of BackendKeyData; a
    /// CancelRequest must echo it to trip `cancel_flag`.
    pub(crate) cancel_secret: u32,
    /// v7.39 — set by a matching CancelRequest; every engine
    /// `cancel.check()` checkpoint sees it through the statement's
    /// CancelToken. Reset when the next statement starts (PG: a
    /// between-statements cancel is a no-op).
    pub(crate) cancel_flag: AtomicBool,
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

    /// v7.37.14 (B6.3) — PG-style wait_event_type categorisation.
    /// Maps the SPG wait_event enum to PG's high-level wait
    /// categories so monitoring dashboards built against
    /// `pg_stat_activity` queries work against SPG without per-
    /// product translation.
    pub(crate) fn wait_event_type_str(&self) -> &'static str {
        match self.wait_event.load(Ordering::Relaxed) {
            1 => "Lock", // write_lock — engine RwLock contention
            2 => "IO",   // fsync — WAL durability stall
            3 => "IPC",  // group_commit — leader/follower coordination
            _ => "",     // idle / no wait
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
        | Err(spg_audit::AuditError::InvalidUtf8 { seq }) => (
            i64::try_from(seq).unwrap_or(i64::MAX),
            i64::try_from(seq).unwrap_or(i64::MAX),
        ),
        Err(_) => (0, 0),
    }
}

/// v6.5.6 — slow-query log callback wired into the engine. Emits
/// a single structured log line per crossing.
pub(crate) fn log_slow_query(sql: &str, elapsed_us: u64) {
    let elapsed_str = elapsed_us.to_string();
    observability::log_event(
        "warn",
        "slow_query",
        &[("elapsed_us", &elapsed_str), ("sql", sql)],
    );
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
            let current_sql = c.current_sql.read().map(|g| g.clone()).unwrap_or_default();
            let application_name = c
                .application_name
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();
            spg_engine::ActivityRow {
                pid: c.pid,
                user: c.user.clone(),
                started_at_us: c.started_at_us,
                current_sql,
                wait_event_type: c.wait_event_type_str().to_string(),
                wait_event: c.wait_event_str().to_string(),
                elapsed_us: c.elapsed_us(),
                in_transaction: c.in_transaction.load(Ordering::Relaxed),
                application_name,
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

/// v7.38 (read01 P5.25) — process-startup hardening. Pins LC_NUMERIC to the
/// C locale (so any linked C code formats numbers with a '.' decimal even
/// under a comma-decimal ambient locale — Rust itself already defaults to C)
/// and warns when the server runs as root. We warn rather than refuse:
/// containers routinely run as root and a hard refusal would break
/// zero-change deployments; the warning still nudges operators toward a
/// dedicated unprivileged user.
#[cfg(unix)]
#[allow(unsafe_code)]
fn startup_hardening() {
    // SAFETY: `setlocale` and `geteuid` are async-signal-safe libc calls with
    // no aliasing or lifetime concerns; the `c"C"` literal is a valid
    // NUL-terminated string with 'static lifetime.
    unsafe {
        libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr());
        if libc::geteuid() == 0 {
            eprintln!(
                "[spg] warning: running as root is discouraged; run as a dedicated \
                 unprivileged user so a database compromise can't escalate to the host."
            );
        }
    }
}

#[cfg(not(unix))]
fn startup_hardening() {}

fn main() {
    // v7.38 (read01 P5.25) — startup hardening: pin LC_NUMERIC=C and warn on
    // running as root.
    startup_hardening();
    // v6.10.4 — peek for `--replay-only` before parsing the
    // positional addr arg. The flag re-targets the boot path:
    // load the catalog snapshot + replay the WAL into the
    // engine, then exit 0 without ever opening a listener.
    // Useful for ops "did the WAL replay cleanly?" smoke tests
    // and for sandboxed forensic restores.
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let replay_only = raw_args.iter().any(|a| a == "--replay-only");
    let mut args = raw_args.into_iter().filter(|a| a != "--replay-only");
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
        max_query_ns: parse_env_u64("SPG_MAX_QUERY_NS"),
        idle_timeout_sec: parse_env_u64("SPG_IDLE_TIMEOUT_SEC"),
        // v7.37.7 (mailrs cascade 4 cycle disable signal) — slowlog
        // is OFF by default in v7.37.6 and earlier, which is how 25
        // budget-breach queries on a single prod cycle remained
        // invisible to the SPG service log. v7.37.7 makes the
        // **default 1000ms** so any prod deploy automatically captures
        // slow queries without operator opt-in. Explicit unsets are
        // still possible via `SPG_SLOW_QUERY_LOG_MS=0`.
        slow_query_log_ms: parse_env_u64("SPG_SLOW_QUERY_LOG_MS").or(Some(1000)),
        wal_min_free_bytes: parse_env_u64("SPG_WAL_MIN_FREE_BYTES"),
        shutdown_deadline_sec: parse_env_u64("SPG_SHUTDOWN_DEADLINE_SEC"),
    };
    install_shutdown_handlers();
    if replay_only {
        if let Err(e) = run_replay_only(db_path, wal_path) {
            eprintln!("spg-server: replay-only fatal: {e}");
            process::exit(1);
        }
        eprintln!("spg-server: --replay-only complete; exiting 0");
        return;
    }
    if let Err(e) = run(&addr, db_path, audit_path, wal_path, password, limits) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

/// v6.10.4 — `--replay-only` boot path. Restores the catalog
/// snapshot at `db_path` (if any) + replays the WAL at
/// `wal_path` (if any) into the engine, then returns. No
/// listener, no audit chain, no replication. The success
/// criterion is "no error reached this layer" — the catalog
/// is dropped at fn exit. Sandboxed by design: a WAL containing
/// poisonous SQL still bubbles up the exec error here, but
/// can't push state into a live deployment.
fn run_replay_only(db_path: Option<PathBuf>, wal_path: Option<PathBuf>) -> std::io::Result<()> {
    let mut engine = match db_path.as_deref() {
        Some(p) if p.exists() => {
            let bytes = fs::read(p)?;
            let engine = Engine::restore_envelope(&bytes)
                .map_err(|e| std::io::Error::other(format!("restore: {e}")))?;
            eprintln!(
                "spg-server: --replay-only restored {} table(s) from {}",
                engine.catalog().table_count(),
                p.display()
            );
            engine
        }
        _ => Engine::new(),
    };
    if let Some(w) = wal_path.as_deref() {
        if w.exists() {
            let wal_bytes = fs::read(w)?;
            let applied = replay_wal_bytes(&wal_bytes, &mut engine)?;
            eprintln!(
                "spg-server: --replay-only applied {applied} WAL record(s) from {}",
                w.display()
            );
        } else {
            eprintln!(
                "spg-server: --replay-only WAL path {} doesn't exist; nothing to replay",
                w.display()
            );
        }
    }
    // Final sanity: take a snapshot so the in-memory state has
    // been exercised through the serialise path before exit.
    let _ = engine.snapshot();
    Ok(())
}

/// Read a usize from `env_key`; non-positive / unparseable / unset
/// becomes `None` (= unlimited). Used by the v4.2 limits envs.
fn parse_env_usize(env_key: &str) -> Option<usize> {
    env_resolve(env_key)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

fn parse_env_u64(env_key: &str) -> Option<u64> {
    env_resolve(env_key)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// v7.37.25 (25.5) — PG GUC name alignment.
///
/// Maps the SPG-spelled env var key to the PG-spelled alias every
/// env reader honours transparently. When both are set the PG-spelled
/// alias wins (the migration target the doc commitment promised);
/// when neither is set returns None — both env vars unset is the
/// same as today.
///
/// Adding a new pair is one line below; the readers (`parse_env_u64`
/// / `parse_env_usize` + any future `_string` variant) inherit
/// resolution automatically.
fn env_resolve(env_key: &str) -> Option<String> {
    // PG-spelled alias → SPG-spelled current key. PG-spelled wins
    // when set on the assumption that an operator who wrote the
    // PG-style name was doing so deliberately.
    const ALIASES: &[(&str, &str)] = &[
        // SPG_STATEMENT_TIMEOUT mirrors PG's statement_timeout GUC,
        // expressed in ms here (PG accepts unit suffixes; SPG keeps
        // its existing bare-integer-ms surface — coercion ships in
        // a follow-up if a customer needs unit-suffix parsing).
        ("SPG_STATEMENT_TIMEOUT", "SPG_QUERY_TIMEOUT_MS"),
        // PG's autovacuum_naptime is in seconds; SPG's surface is
        // ms. PG operators converting will write a milliseconds
        // value here, matching the existing semantics; the GUC name
        // just makes the per-database scrape script obvious.
        ("SPG_AUTOVACUUM_NAPTIME", "SPG_AUTO_ANALYZE_INTERVAL_MS"),
        // PG's log_min_duration_statement is the ms threshold below
        // which queries are not logged. SPG's existing name spells
        // the same semantics with a different label.
        ("SPG_LOG_MIN_DURATION", "SPG_SLOW_QUERY_THRESHOLD_MS"),
    ];
    // Prefer the PG-spelled alias when its mapping owns this key.
    for (pg_name, spg_name) in ALIASES {
        if *spg_name == env_key
            && let Ok(v) = env::var(pg_name)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    env::var(env_key).ok()
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
    // v7.37.25 (25.5) — honours `SPG_AUTOVACUUM_NAPTIME` (PG-aligned
    // alias) ahead of the legacy `SPG_AUTO_ANALYZE_INTERVAL_MS`.
    env_resolve("SPG_AUTO_ANALYZE_INTERVAL_MS")
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
    let (base_engine, manifest_wal_baseline) =
        restore_engine(db_path.as_deref(), &mut cold_segment_paths)?;
    let mut engine = base_engine
        .with_clock(wall_clock_micros)
        .with_salt_fn(urandom_salt_or_panic);

    if let Some(n) = limits.max_query_rows {
        engine = engine.with_max_query_rows(n);
    }

    // v7.30.3 (mailrs round-26) — wire the same SPG_MAX_QUERY_BYTES
    // value into the engine's approximate join-materialisation
    // budget. The allocator-level budget above (`alloc_budget`)
    // stays the precise outer layer; the engine-level meter errors
    // with the actionable QueryBytesExceeded message before the
    // allocator has to trip the cancel flag.
    let engine_byte_cap: Option<usize> = match limits.max_query_bytes {
        Some(0) => None,
        Some(n) => usize::try_from(n).ok(),
        None => usize::try_from(DEFAULT_MAX_QUERY_BYTES).ok(),
    };
    if let Some(n) = engine_byte_cap {
        engine = engine.with_max_query_bytes(n);
    }

    // v7.37.16 — the in-place MVCC write path defaults ON; the env is a
    // two-way override (`SPG_MVCC_INPLACE=0|false|off` reverts to the
    // legacy physical delete). Survives the provider-rebuild swap below
    // (the flag rides the moved engine).
    match std::env::var("SPG_MVCC_INPLACE").ok().as_deref() {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") => {
            engine.set_mvcc_inplace(false);
        }
        Some(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") => {
            engine.set_mvcc_inplace(true);
        }
        _ => {}
    }
    // v7.37.16 — autovacuum defaults ON; `SPG_AUTOVACUUM=0|false|off`
    // disables (operators running their own vacuum cadence).
    if std::env::var("SPG_AUTOVACUUM")
        .is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
    {
        engine.set_autovacuum(false);
    }
    // v7.39 — parallel aggregation defaults ON on the server (the
    // no_std engine can't spawn threads; we inject the executor).
    // `SPG_PARALLEL=0|false|off` reverts to single-threaded scans.
    if !std::env::var("SPG_PARALLEL")
        .is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
    {
        engine.set_parallel_runner(std::sync::Arc::new(ScopedThreadRunner));
    }
    // v7.39 (pg_stat knife A) — pg_stat_database.numbackends reads the
    // live connection count.
    engine.set_backend_count_fn(live_backend_count);
    engine.set_backend_pid_fn(current_backend_pid);
    // v7.39 (tz epic) — named-timezone lookups via the system zoneinfo.
    engine.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );

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

    replay_wal_into_engine(&mut engine, wal_path.as_deref(), manifest_wal_baseline)?;

    bootstrap_admin_from_env(&mut engine, db_path.as_deref())?;

    let (wal, wal_sync_clone) = open_wal_for_append(wal_path.as_deref())?;

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
        async_commit_dirty: AtomicBool::new(false),
    });
    // v6.5.2 — register the global handle so the engine's
    // activity_provider callback can read the live registry. Safe
    // to set unconditionally: ACTIVITY_STATE is a OnceLock with
    // single-set semantics; subsequent server boots in the same
    // process (only relevant for tests) silently keep the first
    // state — engine refs through the static are always live.
    let _ = ACTIVITY_STATE.set(Arc::clone(&state));
    // v6.7.6 — drain the boot-time prefetch hit count into the
    // live metrics (the counter ran before ServerState existed).
    PREFETCH_HITS_BOOT.with(|cell| {
        let hits = cell.take();
        if hits > 0 {
            state
                .metrics
                .cold_prefetch_hits
                .store(hits, std::sync::atomic::Ordering::Relaxed);
        }
    });
    if let Ok(mut e) = state.engine.write() {
        // Replace the engine with one carrying the providers. The
        // builders consume by value, but we can swap in place by
        // taking ownership through std::mem::replace.
        let prev = std::mem::replace(&mut *e, Engine::new());
        // v6.5.6 — slow-query log threshold from env, default 100ms.
        // v7.37.25 (25.5) — honours `SPG_LOG_MIN_DURATION` (PG-aligned
        // alias) ahead of the legacy `SPG_SLOW_QUERY_THRESHOLD_MS`.
        let slow_us: u64 = env_resolve("SPG_SLOW_QUERY_THRESHOLD_MS")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100)
            * 1_000;
        *e = prev
            .with_activity_provider(activity_snapshot)
            .with_audit_providers(audit_chain_snapshot, audit_verify_snapshot)
            .with_slow_query_log(slow_us, log_slow_query);
        // v6.5.6 — operator-tunable plan cache cap.
        if let Ok(s) = std::env::var("SPG_PLAN_CACHE_MAX")
            && let Ok(n) = s.parse::<usize>()
        {
            e.set_plan_cache_max(n);
        }
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

    spawn_optional_listeners(&state);

    // v5.2.2: background freezer. Polls every tick; if hot-tier byte
    // sum exceeds `SPG_HOT_TIER_BYTES` (default 4 GiB), demotes a
    // batch of rows from the largest table with a BTree integer-PK
    // index. Opt-out via `SPG_FREEZER_DISABLE=1` for tests that
    // don't want background mutations under them.
    if freezer::spawn(Arc::clone(&state)).is_none() {
        eprintln!("spg-server: freezer disabled via SPG_FREEZER_DISABLE");
    }

    // v5.4.1: background flusher. In async-commit mode (env
    // `SPG_SYNCHRONOUS_COMMIT=off`, or a session's SQL
    // `SET synchronous_commit = off` after v7.37.15) it emits a
    // v5.4.0 `durability_checkpoint` WAL marker every
    // `SPG_FLUSHER_INTERVAL_US` µs (default 200 µs) so crash
    // recovery can identify how much of the async-commit window
    // had reached fsync at kill time. Always spawned since
    // v7.37.15, but dormant (no markers, no fsync) until async
    // commits actually happen.
    let _ = flusher::spawn(Arc::clone(&state));

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

/// v7.33 (D) — spawn the env-gated auxiliary listeners / workers split
/// out of `run`: the PG-wire and MySQL-wire compatibility listeners, the
/// observability HTTP endpoint, the master-side replication listener, and
/// follower mode. Each is opt-in via its own env var (except the
/// Dockerfile's default `SPG_PG_ADDR`); absent var = no port, no thread.
/// Pure extraction — behaviour is identical to the inline block.
/// v7.33 (D) — restore the engine from the snapshot file (or start
/// fresh), split out of `run`. On an existing db file: restore the v4
/// envelope (or bare v3 catalog), then load the sidecar manifest and
/// auto-preload its cold-tier segments into `cold_segment_paths`,
/// returning the manifest's WAL baseline (0 = replay from start, the
/// legacy default). The clock / salt / query-limit builders are applied
/// by the caller. Pure extraction.
fn restore_engine(
    db_path: Option<&Path>,
    cold_segment_paths: &mut BTreeMap<u32, PathBuf>,
) -> std::io::Result<(Engine, u64)> {
    let mut manifest_wal_baseline: u64 = 0;
    let engine = match db_path {
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
                load_manifest_and_preload_cold(&mut engine, p, &bytes, cold_segment_paths);
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
    };
    Ok((engine, manifest_wal_baseline))
}

/// v7.33 (D) — replay the WAL onto the loaded snapshot, split out of
/// `run`. Honours PITR `SPG_REPLAY_UPTO` (byte-offset cap) and the
/// manifest baseline (skip bytes already folded into the snapshot), and
/// auto-rolls-back an open TX at end-of-WAL. Truncated trailing entries
/// (crash mid-fsync) drop with a warning, not a fatal. When the WAL file
/// is absent it is created empty so the later append open needs no
/// `create` branch. Pure extraction.
fn replay_wal_into_engine(
    engine: &mut Engine,
    wal_path: Option<&Path>,
    manifest_wal_baseline: u64,
) -> std::io::Result<()> {
    // Truncated entries (server crash mid-fsync) abort the loop with a
    // warning rather than fatal — the already-applied prefix wins, the
    // trailing partial entry is dropped. An open TX at end-of-WAL is
    // auto-rolled-back.
    if let Some(p) = wal_path
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
        let applied = replay_wal_bytes(&bytes, engine)?;
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
    } else if let Some(p) = wal_path {
        // Create empty WAL file ahead of time so OpenOptions::append
        // (open_wal_for_append) doesn't need a `create(true)` branch.
        fs::write(p, b"")?;
        eprintln!("spg-server: started fresh WAL at {}", p.display());
    }
    Ok(())
}

/// v7.33 (D) — open the WAL file for append + clone its handle for the
/// async-commit flusher's lock-free fsync, split out of `run`. `None`
/// path = WAL disabled. Pure extraction.
#[allow(clippy::type_complexity)]
fn open_wal_for_append(
    wal_path: Option<&Path>,
) -> std::io::Result<(Option<Mutex<File>>, Option<Arc<File>>)> {
    match wal_path {
        Some(p) => {
            let file = OpenOptions::new().append(true).open(p).map_err(|e| {
                std::io::Error::other(format!("open WAL {} for append: {e}", p.display()))
            })?;
            // v5.4.4 — clone the handle for lock-free fsync from the
            // async-commit flusher. `try_clone` failure (extremely
            // rare; would mean fd exhaustion at startup) degrades
            // gracefully: the flusher falls back to taking the mutex.
            let sync_clone = file.try_clone().ok().map(Arc::new);
            Ok((Some(Mutex::new(file)), sync_clone))
        }
        None => Ok((None, None)),
    }
}

fn spawn_optional_listeners(state: &Arc<ServerState>) {
    // v4.3: optional PG-wire compatibility listener. Opt-in via env
    // so a deployment that doesn't need psql / Metabase / DBeaver
    // doesn't pay the extra port + thread.
    // v7.13.0 (C1, mailrs round-5): the Dockerfile sets
    // `SPG_PG_ADDR=0.0.0.0:5432` so the official image is a
    // drop-in PG listener. Local / cargo-test runs keep the
    // pre-v7.13 opt-in behaviour (no listener unless the env var
    // is explicitly set).
    if let Ok(pg_addr) = env::var("SPG_PG_ADDR")
        && !pg_addr.is_empty()
    {
        match pgwire::spawn_listener(&pg_addr, Arc::clone(state)) {
            Ok(pg_local) => eprintln!("spg-server: pg-wire listening on {pg_local}"),
            Err(e) => eprintln!("spg-server: pg-wire failed to start on {pg_addr}: {e}"),
        }
    }

    // v7.17.0 Phase 3.P0-70 — optional MySQL-wire compatibility
    // listener. Opt-in via env so existing deployments don't
    // suddenly grow a third bound port. Plumbed through Segment
    // G as auth (P0-71/P0-72), commands (P0-73..P0-75), binary
    // result rows (P0-76), and SSL upgrade (P0-77) land.
    if let Ok(my_addr) = env::var("SPG_MYSQLWIRE_ADDR")
        && !my_addr.is_empty()
    {
        match mysqlwire::spawn_listener(&my_addr, Arc::clone(state)) {
            Ok(my_local) => eprintln!("spg-server: mysql-wire listening on {my_local}"),
            Err(e) => eprintln!("spg-server: mysql-wire failed to start on {my_addr}: {e}"),
        }
    }

    // v4.13: optional observability HTTP endpoint. /healthz for
    // k8s liveness, /metrics for Prometheus scraping. The
    // listener reads live counters out of state directly via
    // an Arc<ServerState>.
    if let Ok(http_addr) = env::var("SPG_HTTP_ADDR")
        && !http_addr.is_empty()
    {
        match observability::spawn_http(&http_addr, Arc::clone(state)) {
            Ok(http_local) => eprintln!("spg-server: http listening on {http_local}"),
            Err(e) => eprintln!("spg-server: http failed to start on {http_addr}: {e}"),
        }
    }

    // v4.24: optional master-side replication listener. When set,
    // followers can connect and stream the WAL.
    if let Ok(repl_addr) = env::var("SPG_REPL_ADDR")
        && !repl_addr.is_empty()
    {
        match replication::spawn_master_listener(&repl_addr, Arc::clone(state)) {
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
            let state_for_follower = Arc::clone(state);
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
/// v7.39 (pg_stat knife A) — process-wide mirror of
/// `ServerState.active_connections` for the engine's
/// `BackendCountFn` slot (a plain fn pointer can't capture the Arc).
/// Kept in lock-step by ConnectionGuard claim/drop.
static BACKEND_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn live_backend_count() -> u32 {
    BACKEND_COUNT.load(Ordering::Relaxed)
}

// v7.39 (read01 pgstatfuncs.c) — per-connection identity for
// pg_backend_pid(). Each pgwire connection runs on its own thread and
// stamps its ConnState.pid here at session start; the engine's
// BackendPidFn slot reads it back during evaluation on that thread.
thread_local! {
    static CONN_PID: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

pub(crate) fn set_conn_pid(pid: u32) {
    CONN_PID.with(|c| c.set(pid));
}

fn current_backend_pid() -> u32 {
    CONN_PID.with(std::cell::Cell::get)
}

pub(crate) fn backend_count_incr() {
    BACKEND_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn backend_count_decr() {
    BACKEND_COUNT.fetch_sub(1, Ordering::Relaxed);
}

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
                BACKEND_COUNT.fetch_add(1, Ordering::Relaxed);
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
        BACKEND_COUNT.fetch_sub(1, Ordering::Relaxed);
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
fn handle_show_wal_level(stream: &mut TcpStream, state: &Arc<ServerState>) -> std::io::Result<()> {
    let level = state.wal_level.load(Ordering::Acquire);
    let row = vec![Row::new(vec![Value::text(wal_level_label(level))])];
    let columns = vec![ColumnSchema::new(
        "effective_wal_level",
        DataType::Text,
        false,
    )];
    emit_result(
        stream,
        Ok(spg_engine::QueryResult::Rows { columns, rows: row }),
    )
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
    let deadline =
        timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    loop {
        let current = state.lag_state.follower_applied_pos.load(Ordering::Acquire);
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
        Op::Query => handle_query_op(stream, frame, state, *role, in_tx),
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

/// v7.33 (D) — the native-wire `Op::Query` handler, split out of
/// `dispatch` (it was ~330 of its ~430 lines). Parses the SQL, runs
/// the read fast path / write commit-barrier path, and applies WAL +
/// snapshot + manifest + audit + subscription reconcile. Pure
/// extraction; `dispatch` now just authenticates + routes opcodes.
fn handle_query_op(
    stream: &mut TcpStream,
    frame: &Frame,
    state: &Arc<ServerState>,
    role: Option<Role>,
    in_tx: &mut bool,
) -> std::io::Result<()> {
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
    let _slow_log = SlowLogGuard::new(state, &sql, role);
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
        let engine = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        // v7.37.7 — read session `statement_timeout` GUC (PG semantics)
        // BEFORE spawning the watchdog so a SQL-level
        // `SET statement_timeout = N` on this connection takes effect
        // for the current query. The watchdog picks the tighter of
        // env-var SPG_QUERY_TIMEOUT_MS and session GUC.
        let session_timeout = engine.session_statement_timeout_ms();
        let watchdog = spawn_query_watchdog(state, &cancel_flag, session_timeout);
        let budget = usize::try_from(
            state
                .limits
                .max_query_bytes
                .unwrap_or(DEFAULT_MAX_QUERY_BYTES),
        )
        .unwrap_or(usize::MAX);
        alloc_budget::reset_query_budget(budget, &cancel_flag);
        let result = engine
            .execute_readonly_with_cancel(&sql, spg_engine::CancelToken::from_flag(&cancel_flag));
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
    let acting = current_role(role);
    if sql_is_user_mgmt(&sql) {
        if !acting.can_manage_users() {
            return write_frame(
                stream,
                &build_error_response("permission denied: user management requires admin role"),
            );
        }
    } else if !acting.can_write() {
        return write_frame(
            stream,
            &build_error_response("permission denied: write requires admin or readwrite role"),
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
    // v6.7.3: intercept COMPACT COLD SEGMENTS. Engine-level
    // execution would only mutate the catalog in memory;
    // server-side persists each merged segment to
    // `<db>.spg/segments/seg_<merged_id>.spg` + updates
    // `cold_segment_paths` so the next CHECKPOINT writes a
    // manifest that no longer lists the retired sources.
    // Admin-only — same operator-surface as CHECKPOINT.
    if parse_compact_cold_segments_intent(&sql) {
        if !acting.can_manage_users() {
            return write_frame(
                stream,
                &build_error_response(
                    "permission denied: COMPACT COLD SEGMENTS requires admin role",
                ),
            );
        }
        return run_compact_cold_segments_command(stream, state);
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
    // v7.37.7 — session `statement_timeout` GUC also applies to writes
    // (PG semantics). Read it before spawning the watchdog so a SQL
    // `SET statement_timeout = N` honoured uniformly across read +
    // write paths. RwLock read-borrow is dropped before the wrap path's
    // write leader needs the lock.
    let session_timeout_ms = {
        let engine_guard = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        engine_guard.session_statement_timeout_ms()
    };
    // v7.37.15 (r172) — effective synchronous_commit, captured here
    // (read lock, before the write paths below take theirs) for the
    // same reason as `session_timeout_ms`. Both the wrap path (leader
    // group fsync) and the non-wrap path (per-statement append) honour
    // it, matching PG: `SET synchronous_commit = off` commits return
    // before fsync, with the flusher bounding the loss window.
    let sync_commit = session_sync_commit(state);
    let watchdog = spawn_query_watchdog(state, &cancel_flag, session_timeout_ms);
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
            sync_commit,
        };
        let became_leader = enqueue_commit_task(state, task);
        if became_leader {
            run_leader_commit_round(state);
        }
        let CommitResult {
            result,
            wal_outcome,
        } = ack_rx.recv().map_err(|_| {
            std::io::Error::other("commit barrier: ack channel closed before result arrived")
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
        let result =
            engine.execute_with_cancel(&sql, spg_engine::CancelToken::from_flag(&cancel_flag));
        alloc_budget::clear_query_budget();
        let was_command_ok = matches!(result, Ok(QueryResult::CommandOk { .. }));
        let wal_result = if was_command_ok && state.wal.is_some() {
            // `sync_commit` was captured before this branch took the
            // engine write lock — `append_wal` can't read the session
            // GUC itself here without self-deadlocking on the RwLock.
            append_wal(state, &sql, sync_commit)
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
    // v7.13.0 (C2/C3/C4, mailrs round-5): the official PG image
    // bootstraps via POSTGRES_USER / POSTGRES_PASSWORD /
    // POSTGRES_DB env vars. SPG is single-database (POSTGRES_DB
    // has no on-disk effect — every connection sees the same
    // single catalog) but we accept and log it so docker-compose
    // files port over verbatim.
    let pg_user = env::var("POSTGRES_USER").ok().filter(|s| !s.is_empty());
    let pg_pass = env::var("POSTGRES_PASSWORD").ok().filter(|s| !s.is_empty());
    if let Some(db) = env::var("POSTGRES_DB").ok().filter(|s| !s.is_empty()) {
        eprintln!(
            "spg-server: POSTGRES_DB={db:?} accepted — SPG is single-database, \
             every connection sees the same catalog"
        );
    }
    let (pw, user) = match (
        env::var("SPG_ADMIN_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty()),
        pg_pass,
    ) {
        (Some(spg_pw), _) => {
            let user = env::var("SPG_ADMIN_USER")
                .ok()
                .filter(|s| !s.is_empty())
                .or(pg_user)
                .unwrap_or_else(|| "admin".to_string());
            (spg_pw, user)
        }
        (None, Some(pg_pw)) => {
            let user = pg_user.unwrap_or_else(|| "postgres".to_string());
            (pg_pw, user)
        }
        (None, None) => return Ok(()),
    };
    let salt = random_salt()?;
    engine
        .create_user(&user, &pw, Role::Admin, salt)
        .map_err(|e| std::io::Error::other(format!("bootstrap admin {user:?}: {e}")))?;
    eprintln!("spg-server: bootstrapped admin user {user:?} from env");
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

fn spawn_query_watchdog(
    state: &ServerState,
    cancel_flag: &Arc<AtomicBool>,
    session_timeout_ms: Option<u64>,
) -> Watchdog {
    let completed = Arc::new(AtomicBool::new(false));
    // v6.10.1 — pick the tighter of `query_timeout_ms` and
    // `max_query_ns` (converted to a Duration). `None` from one
    // side defers to the other; both `None` → no budget.
    let timeout_dur = state
        .limits
        .query_timeout_ms
        .map(std::time::Duration::from_millis);
    let cpu_dur = state
        .limits
        .max_query_ns
        .map(std::time::Duration::from_nanos);
    // v7.37.7 — session `SET statement_timeout = N` is the third input.
    // The session-set value (in ms) is read by the caller via
    // `Engine::session_statement_timeout_ms()` and passed in here so a
    // SQL-level GUC change takes effect for the very next statement on
    // the same connection. PG semantics: per-session GUC + cluster
    // (env-var) bound, the tighter wins.
    let session_dur = session_timeout_ms.map(std::time::Duration::from_millis);
    let total = [timeout_dur, cpu_dur, session_dur]
        .into_iter()
        .flatten()
        .min();
    let Some(total) = total else {
        return Watchdog { completed };
    };
    let cancel_flag = Arc::clone(cancel_flag);
    let completed_for_thread = Arc::clone(&completed);
    thread::spawn(move || {
        // Sleep in short slices so a finished query reclaims the
        // watchdog quickly (avoids piling up parked threads when
        // the budget is high but queries are usually fast). v6.10.1
        // shortens the slice to 100µs ceiling so sub-ms budgets
        // (via SPG_MAX_QUERY_NS) fire on time.
        let slice = (total / 50).max(std::time::Duration::from_micros(100));
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
    // v7.39 (round 147, durable-rename audit) — fsync the tmp bytes before
    // the rename and the directory entry after it, so a power loss cannot
    // leave the final name pointing at unflushed (empty / torn) content or
    // lose the rename itself. Mirrors the embedded snapshot path and PG's
    // durable_rename.
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(dir);
    Ok(())
}

/// v7.39 (round 147, durable-rename audit) — fsync a directory so a
/// just-renamed entry survives power loss. `std::fs::rename` makes the new
/// name visible to this process but does not flush the directory inode.
/// Best-effort (matches the embedded twin): an unreadable dir is not fatal.
pub(crate) fn fsync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
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
pub(crate) fn write_manifest_alongside(
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
    // v6.7.6 — parallel prefetch. Read every segment file off disk
    // in a `SPG_PREFETCH_WORKERS`-wide thread pool before the
    // sequential CRC + register loop runs.
    let paths_to_read: Vec<(u32, PathBuf)> = m
        .cold_segments
        .iter()
        .map(|e| (e.segment_id, e.path.clone()))
        .collect();
    let workers = prefetch::worker_count_from_env();
    let prefetched = prefetch::parallel_read_segments(&paths_to_read, workers, None);
    let read_map: std::collections::BTreeMap<u32, std::io::Result<Vec<u8>>> =
        prefetched.into_iter().collect();
    let mut prefetch_hits: u64 = 0;
    for entry in &m.cold_segments {
        let bytes_result = read_map
            .get(&entry.segment_id)
            .map(|r| match r {
                Ok(b) => Ok(b.clone()),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            })
            .unwrap_or_else(|| {
                Err(std::io::Error::other(format!(
                    "no prefetch result for segment {}",
                    entry.segment_id
                )))
            });
        match bytes_result {
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
                // v6.7.3 — load at the manifest-baked id so a
                // tombstoned-but-not-yet-orphan-GC'd source
                // segment doesn't shift the surviving ids; the
                // BTree-index `RowLocator::Cold { segment_id }`
                // baked into the catalog snapshot must continue
                // to resolve byte-identically across the bounce.
                match cat.load_segment_bytes_at(entry.segment_id, seg_bytes) {
                    Ok(()) => {
                        cold_segment_paths.insert(entry.segment_id, entry.path.clone());
                        loaded += 1;
                        prefetch_hits += 1;
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
    // Stash on a thread-local so the post-boot `state.metrics`
    // can claim the counter once ServerState is built.
    PREFETCH_HITS_BOOT.with(|cell| cell.set(prefetch_hits));
    eprintln!(
        "spg-server: manifest {} loaded {loaded} cold segment(s), skipped {skipped}; wal_baseline_offset={}",
        mp.display(),
        m.wal_baseline_offset,
    );
    m.wal_baseline_offset
}

/// v7.39 (parallel-agg P0) — std-side ParallelRunner: scoped threads,
/// one per shard. Shard counts are small (<= 8) and gated to scans of
/// 100k+ rows, so per-query spawn cost (~10-20 us/thread) is noise
/// next to the scan itself; a pooled runner is a P3 refinement.
struct ScopedThreadRunner;

impl spg_engine::ParallelRunner for ScopedThreadRunner {
    fn run_shards(
        &self,
        n: usize,
        f: &(dyn Fn(usize) -> Box<dyn core::any::Any + Send> + Sync),
    ) -> Vec<Box<dyn core::any::Any + Send>> {
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n).map(|i| s.spawn(move || f(i))).collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("shard panicked"))
                .collect()
        })
    }
}
