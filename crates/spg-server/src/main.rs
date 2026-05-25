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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use spg_audit::AuditLog;
use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, Value};
use spg_wire::{
    ColumnDesc, Frame, FrameError, Op, WireType, WireValue, build_command_complete, build_data_row,
    build_error_response, build_row_description, decode, encode, parse_query,
};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_CHUNK: usize = 4096;

struct ServerState {
    engine: Mutex<Engine>,
    db_path: Option<PathBuf>,
    audit_log: Mutex<AuditLog>,
    audit_path: Option<PathBuf>,
    wal: Option<Mutex<File>>,
    /// Kept so the path can be reported in error messages; runtime appends go
    /// through `wal` directly.
    _wal_path: Option<PathBuf>,
}

fn parse_optional_path(arg: Option<String>) -> Option<PathBuf> {
    arg.filter(|s| !s.is_empty() && s != "-").map(PathBuf::from)
}

fn main() {
    let mut args = env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = parse_optional_path(args.next());
    let audit_path = parse_optional_path(args.next());
    let wal_path = parse_optional_path(args.next());
    if let Err(e) = run(&addr, db_path, audit_path, wal_path) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

fn run(
    addr: &str,
    db_path: Option<PathBuf>,
    audit_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
) -> std::io::Result<()> {
    let mut engine = match &db_path {
        Some(p) if p.exists() => {
            let bytes = fs::read(p)?;
            let path_str = p.display();
            let catalog = Catalog::deserialize(&bytes)
                .map_err(|e| std::io::Error::other(format!("restore from {path_str}: {e}")))?;
            eprintln!(
                "spg-server: restored {} table(s) from {path_str}",
                catalog.table_count()
            );
            Engine::restore(catalog)
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

    let wal = match &wal_path {
        Some(p) => Some(Mutex::new(
            OpenOptions::new().append(true).open(p).map_err(|e| {
                std::io::Error::other(format!("open WAL {} for append: {e}", p.display()))
            })?,
        )),
        None => None,
    };

    let state = Arc::new(ServerState {
        engine: Mutex::new(engine),
        db_path,
        audit_log: Mutex::new(audit_log),
        audit_path,
        wal,
        _wal_path: wal_path,
    });

    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("spg-server: listening on {local}");

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
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];

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
                    dispatch(&mut stream, &frame, state)?;
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

fn dispatch(stream: &mut TcpStream, frame: &Frame, state: &ServerState) -> std::io::Result<()> {
    match frame.op {
        Op::Ping => write_frame(stream, &Frame::pong()),
        Op::Query => {
            let sql = match parse_query(frame) {
                Ok(s) => s.to_string(),
                Err(e) => return write_frame(stream, &build_error_response(&e.to_string())),
            };
            let (result, snapshot) = {
                let mut engine = state
                    .engine
                    .lock()
                    .map_err(|_| std::io::Error::other("engine mutex poisoned"))?;
                let result = engine.execute(&sql);
                // Snapshot only when the committed catalog actually changed —
                // i.e. for COMMIT and for writes outside a TX. Intra-TX writes
                // and BEGIN/ROLLBACK never trigger persistence. Additionally,
                // when WAL is on, we don't update the db snapshot at runtime:
                // the WAL captures every op and the db file is checkpoint-only.
                let snap = match (&result, state.wal.is_some()) {
                    (
                        Ok(QueryResult::CommandOk {
                            modified_catalog: true,
                            ..
                        }),
                        false,
                    ) => Some(engine.snapshot()),
                    _ => None,
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
            // Audit-log only when the committed state actually changed. For
            // a TX, this means one audit entry on COMMIT (its SQL text is
            // "COMMIT" — the body of the TX is recorded only implicitly).
            if matches!(
                result,
                Ok(QueryResult::CommandOk {
                    modified_catalog: true,
                    ..
                })
            ) && let Err(e) = append_audit(state, &sql)
            {
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("audit append failed: {e}")),
                );
                return Err(e);
            }
            emit_result(stream, result)
        }
        Op::Pong | Op::RowDescription | Op::DataRow | Op::CommandComplete | Op::ErrorResponse => {
            write_frame(
                stream,
                &Frame::error("client → server opcode not accepted on this side"),
            )
        }
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
            let descs = columns
                .iter()
                .map(column_schema_to_desc)
                .collect::<Vec<_>>();
            let rd =
                build_row_description(&descs).map_err(|e| std::io::Error::other(e.to_string()))?;
            write_frame(stream, &rd)?;
            for row in rows {
                let wire = row_to_wire(&row);
                let frame =
                    build_data_row(&wire).map_err(|e| std::io::Error::other(e.to_string()))?;
                write_frame(stream, &frame)?;
            }
            write_frame(stream, &build_command_complete(0))
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
        DataType::Int => WireType::Int,
        DataType::BigInt => WireType::BigInt,
        DataType::Float => WireType::Float,
        DataType::Text => WireType::Text,
        DataType::Bool => WireType::Bool,
    }
}

fn row_to_wire(r: &Row) -> Vec<WireValue> {
    r.values.iter().map(value_to_wire).collect()
}

fn value_to_wire(v: &Value) -> WireValue {
    match v {
        Value::Null => WireValue::Null,
        Value::Int(n) => WireValue::Int(*n),
        Value::BigInt(n) => WireValue::BigInt(*n),
        Value::Float(x) => WireValue::Float(*x),
        Value::Text(s) => WireValue::Text(s.clone()),
        Value::Bool(b) => WireValue::Bool(*b),
    }
}

fn write_frame(stream: &mut TcpStream, frame: &Frame) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode(frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&out)
}
