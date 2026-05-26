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

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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

struct ServerState {
    /// v4.0: `RwLock` instead of `Mutex` so read-only statements
    /// (SELECT / SHOW outside an active TX) can run in parallel
    /// across connections. The write path takes `.write()`; the
    /// read path takes `.read()` and uses `Engine::execute_readonly`.
    engine: RwLock<Engine>,
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
    if let Err(e) = run(&addr, db_path, audit_path, wal_path, password) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

#[allow(clippy::too_many_lines)] // startup wires snapshot+audit+WAL+bootstrap; splitting scatters init logic
fn run(
    addr: &str,
    db_path: Option<PathBuf>,
    audit_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    password: Option<String>,
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
        let bytes = fs::read(p)?;
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
    let state = Arc::new(ServerState {
        engine: RwLock::new(engine),
        db_path,
        audit_log: Mutex::new(audit_log),
        audit_path,
        wal,
        wal_path,
        password,
    });

    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("spg-server: listening on {local}{auth_msg}");

    for stream in listener.incoming() {
        let stream = stream?;
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(stream, &state) {
                eprintln!("spg-server: conn {peer:?}: {e}");
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
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
        let n = stream.read(&mut chunk)?;
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
            let sql = match parse_query(frame) {
                Ok(s) => s.to_string(),
                Err(e) => return write_frame(stream, &build_error_response(&e.to_string())),
            };
            // v4.0 fast path: SELECT / SHOW outside an active TX take
            // the engine *read* lock and run in parallel with other
            // readers. WriteRequired drop-through is rare (only if
            // `sql_is_read_only` peek mis-classifies — over-broad
            // matches like a column named "select" don't happen in
            // practice).
            if !*in_tx && sql_is_read_only(&sql) {
                let engine = state
                    .engine
                    .read()
                    .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
                let result = engine.execute_readonly(&sql);
                drop(engine);
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
            let (result, snapshot) = {
                let mut engine = state
                    .engine
                    .write()
                    .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
                let result = engine.execute(&sql);
                // v4.0: sync per-conn TX state from the engine.
                // Engine.in_transaction() is the authoritative bit
                // (it knows whether BEGIN succeeded, whether a nested
                // SAVEPOINT was opened, whether COMMIT/ROLLBACK left
                // us outside a TX, etc.).
                *in_tx = engine.in_transaction();
                // Snapshot only when the committed catalog actually changed —
                // i.e. for COMMIT and for writes outside a TX. Intra-TX writes
                // and BEGIN/ROLLBACK never trigger persistence. Additionally,
                // when WAL is on, we don't update the db snapshot at runtime:
                // the WAL captures every op and the db file is checkpoint-only.
                //
                // v3.4.0 fix: also skip `engine.snapshot()` when
                // `db_path` is None — the bytes would have been
                // discarded immediately. Without this gate, every
                // write allocates+frees a fresh full-catalog
                // serialisation (catalog 3 MB per snapshot × 110K
                // writes = ~330 GB of allocator churn, observable as
                // a 400+ MB RSS spike).
                let snap = if state.db_path.is_some() && state.wal.is_none() {
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
                (result, snap)
            };
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
            // Append every successful CommandOk SQL to the WAL (when WAL is on).
            // This captures BEGIN/intra-TX/COMMIT/ROLLBACK literally so replay
            // reconstructs state via engine.execute().
            if matches!(result, Ok(QueryResult::CommandOk { .. }))
                && let Err(e) = append_wal(state, &sql)
            {
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
    f.write_all(&entry)?;
    f.sync_data()?;
    Ok(())
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
fn wall_clock_micros() -> i64 {
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
        | DataType::Interval => WireType::Text,
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
        Value::Text(s) => WireValue::Text(s.clone()),
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
