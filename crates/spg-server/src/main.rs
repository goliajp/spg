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

mod backup;
mod observability;
mod pgwire;
mod replication;
mod scram;

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Kept so the path can be reported in error messages; runtime appends go
    /// through `wal` directly.
    wal_path: Option<PathBuf>,
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
    /// v4.29: optional failure-injection knobs used by chaos tests.
    /// All branches default-off and skip the check entirely when
    /// the env var wasn't set on startup.
    chaos: ChaosKnobs,
    /// v4.36: follower-side replication lag tracking. Populated by
    /// `replication::run_follower` when this node negotiated the v2
    /// protocol (status frames). On a primary or a v1-only follower
    /// these stay at zero and `/metrics` omits the series.
    pub(crate) lag_state: Arc<replication::LagState>,
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

#[allow(clippy::too_many_lines)] // startup wires snapshot+audit+WAL+bootstrap; splitting scatters init logic
fn run(
    addr: &str,
    db_path: Option<PathBuf>,
    audit_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    password: Option<String>,
    limits: Limits,
) -> std::io::Result<()> {
    let mut engine = match &db_path {
        Some(p) if p.exists() => {
            let bytes = fs::read(p)?;
            let path_str = p.display();
            // v4.1: snapshot may be either a v4.1 envelope (catalog +
            // users) or the bare v3.x catalog blob. `restore_envelope`
            // handles both transparently — v3.x files keep loading.
            let engine = Engine::restore_envelope(&bytes)
                .map_err(|e| std::io::Error::other(format!("restore from {path_str}: {e}")))?;
            eprintln!(
                "spg-server: restored {} table(s), {} user(s) from {path_str}",
                engine.catalog().table_count(),
                engine.users().len()
            );
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

    let wal = match &wal_path {
        Some(p) => Some(Mutex::new(
            OpenOptions::new().append(true).open(p).map_err(|e| {
                std::io::Error::other(format!("open WAL {} for append: {e}", p.display()))
            })?,
        )),
        None => None,
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
    let state = Arc::new(ServerState {
        engine: RwLock::new(engine),
        db_path,
        audit_log: Mutex::new(audit_log),
        audit_path,
        wal,
        wal_path,
        password,
        limits,
        active_connections: AtomicUsize::new(0),
        metrics: Arc::new(observability::Metrics::default()),
        chaos,
        lag_state: Arc::new(replication::LagState::default()),
    });

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

fn handle(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
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

#[allow(clippy::too_many_lines)] // big dispatch table, splitting would scatter the per-op gates
fn dispatch(
    stream: &mut TcpStream,
    frame: &Frame,
    state: &ServerState,
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
                let result = engine.execute_readonly_with_cancel(
                    &sql,
                    spg_engine::CancelToken::from_flag(&cancel_flag),
                );
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
            // v4.34: when WAL is on and this is an auto-commit write
            // (no client-driven TX in flight, not a TX-control verb),
            // wrap the engine mutation in an implicit BEGIN..COMMIT
            // and append the whole [BEGIN, sql, COMMIT] block to the
            // WAL with one atomic fsync. If the WAL append fails, we
            // ROLLBACK the implicit TX — the live in-memory state
            // never sees the half-applied write. Closes the real
            // ENOSPC mid-`write_all` window that v4.30's preflight
            // chaos path couldn't fix on its own (PROD_READY 1.11).
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
                    wal_block_size(&["BEGIN", &sql, "COMMIT"])
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
            // Hold the engine write lock across the wrap so the
            // sequence BEGIN → execute → WAL → COMMIT/ROLLBACK is
            // serialized with other writers. Readers (read lock)
            // are already blocked while we hold .write(), so the
            // implicit tx_catalog is never observable.
            let mut engine = state
                .engine
                .write()
                .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
            if needs_wrap && let Err(e) = engine.execute("BEGIN") {
                drop(engine);
                watchdog.cancel();
                return write_frame(
                    stream,
                    &build_error_response(&format!("implicit BEGIN failed: {e}")),
                );
            }
            let result =
                engine.execute_with_cancel(&sql, spg_engine::CancelToken::from_flag(&cancel_flag));
            let was_command_ok = matches!(result, Ok(QueryResult::CommandOk { .. }));
            // v4.34: WAL append happens *under the engine lock* now
            // so the BEGIN..COMMIT atomicity holds without another
            // writer slipping in. Cost: WAL fsync stretches the
            // write critical section. The previous design dropped the
            // lock before WAL — but we couldn't roll the implicit TX
            // back without re-locking, and re-locking re-opens the
            // race where in-memory state diverges.
            let wal_result = if was_command_ok && state.wal.is_some() {
                if needs_wrap {
                    append_wal_atomic_block(state, &["BEGIN", &sql, "COMMIT"])
                } else {
                    append_wal(state, &sql)
                }
            } else {
                Ok(())
            };
            if needs_wrap {
                let outcome_sql = if was_command_ok && wal_result.is_ok() {
                    "COMMIT"
                } else {
                    "ROLLBACK"
                };
                if let Err(e) = engine.execute(outcome_sql) {
                    drop(engine);
                    watchdog.cancel();
                    return write_frame(
                        stream,
                        &build_error_response(&format!("implicit {outcome_sql} failed: {e}")),
                    );
                }
            }
            // v4.0: sync per-conn TX state from the engine.
            *in_tx = engine.in_transaction();
            // Snapshot only when the committed catalog actually changed
            // and WAL is off (snapshot replaces WAL as the durability
            // surface in that mode). With WAL on, the implicit COMMIT
            // is already captured in the WAL block above.
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
            watchdog.cancel();
            // Snapshot the catalog first; an audit entry that survives a
            // partial flush would be inconsistent.
            if let (Some(bytes), Some(path)) = (snapshot, state.db_path.as_deref())
                && let Err(e) = write_atomic(path, &bytes)
            {
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("snapshot write failed: {e}")),
                );
                return Err(e);
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

/// v4.34: serialized byte count an atomic block will occupy in the
/// WAL (`4 + len(sql)` per entry, summed). Used by the preflight
/// quota check so the rejection decision matches what the actual
/// `append_wal_atomic_block` would write.
fn wal_block_size(sqls: &[&str]) -> u64 {
    sqls.iter().map(|s| 4u64 + s.len() as u64).sum()
}

/// v4.34: append several length-prefixed WAL entries with **one**
/// `write_all` + **one** `fsync`. Used for the auto-commit
/// BEGIN..COMMIT wrap so the per-write fsync count stays at 1 (vs.
/// 3 for naive per-statement appends).
///
/// Atomic in the operational sense: the OS may still write a torn
/// suffix if the disk fails partway, but the existing length-prefix
/// framing + WAL-replay tail-drop (v4.29 row 1.9) handles that —
/// replay sees zero or more whole entries and stops cleanly at the
/// first truncated one. The implicit-TX `ROLLBACK` in the dispatch
/// path then undoes the engine-side state, keeping in-memory and
/// disk consistent.
fn append_wal_atomic_block(state: &ServerState, sqls: &[&str]) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    let total_len: usize = sqls.iter().map(|s| 4 + s.len()).sum();
    let mut batch = Vec::with_capacity(total_len);
    for sql in sqls {
        let len = u32::try_from(sql.len())
            .map_err(|_| std::io::Error::other("SQL too large for WAL entry"))?;
        batch.extend_from_slice(&len.to_le_bytes());
        batch.extend_from_slice(sql.as_bytes());
    }
    let mut f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    if let Some(quota) = state.chaos.wal_quota_bytes {
        let current = f.metadata().map_or(0, |m| m.len());
        if current.saturating_add(batch.len() as u64) > quota {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded: cur={current} + {} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)",
                    batch.len()
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
    f.write_all(&batch)?;
    f.sync_data()?;
    Ok(())
}

fn append_wal(state: &ServerState, sql: &str) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    let len = u32::try_from(sql.len())
        .map_err(|_| std::io::Error::other("SQL too large for WAL entry"))?;
    let mut entry = Vec::with_capacity(4 + sql.len());
    entry.extend_from_slice(&len.to_le_bytes());
    entry.extend_from_slice(sql.as_bytes());
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
    f.sync_data()?;
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
/// A truncated trailing entry (e.g. crash mid-append) is dropped with a
/// warning to stderr. Non-truncation errors (engine rejected the SQL, bad
/// UTF-8) are fatal — the operator must inspect.
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
        let len_arr: [u8; 4] = bytes[cur..cur + 4].try_into().expect("checked");
        let len = u32::from_le_bytes(len_arr) as usize;
        cur += 4;
        if cur + len > bytes.len() {
            eprintln!("spg-server: WAL entry truncated (sql_len={len}) — dropping tail");
            break;
        }
        let sql = core::str::from_utf8(&bytes[cur..cur + len])
            .map_err(|_| std::io::Error::other("WAL entry has non-UTF-8 SQL"))?;
        engine
            .execute(sql)
            .map_err(|e| std::io::Error::other(format!("WAL replay rejected {sql:?}: {e}")))?;
        cur += len;
        applied += 1;
    }
    Ok(applied)
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
    if let Some(p) = db_path
        && let Err(e) = write_atomic(p, &engine.snapshot())
    {
        eprintln!("spg-server: warning — failed to persist bootstrap admin: {e}");
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
        DataType::Vector(_) => WireType::Vector,
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
