//! SPG daemon — TCP listener that accepts wire frames and dispatches them.
//!
//! v0.6 adds optional persistence: pass a second CLI arg with a file path and
//! the daemon will restore the catalog from it on startup and atomically
//! snapshot after every successful DDL / DML statement.
//!
//! ```text
//! spg-server [addr] [db_path]
//! ```
//!
//! Both arguments are optional. `addr` defaults to `127.0.0.1:5544`. When
//! `db_path` is absent the engine is fully in-memory.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
}

fn main() {
    let mut args = env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = args.next().map(PathBuf::from);
    if let Err(e) = run(&addr, db_path) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

fn run(addr: &str, db_path: Option<PathBuf>) -> std::io::Result<()> {
    let engine = match &db_path {
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

    let state = Arc::new(ServerState {
        engine: Mutex::new(engine),
        db_path,
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
            // Lock once: execute + (if writeful) take a snapshot byte buffer
            // under the same lock so the file matches a definite state.
            let (result, snapshot) = {
                let mut engine = state
                    .engine
                    .lock()
                    .map_err(|_| std::io::Error::other("engine mutex poisoned"))?;
                let result = engine.execute(&sql);
                let snap = match &result {
                    Ok(QueryResult::CommandOk { .. }) => Some(engine.snapshot()),
                    _ => None,
                };
                (result, snap)
            };
            if let (Some(bytes), Some(path)) = (snapshot, state.db_path.as_deref())
                && let Err(e) = write_atomic(path, &bytes)
            {
                // Report the snapshot failure to the client AND propagate up so
                // the connection is torn down — better fail loud than silently
                // lose durability guarantees.
                let _ = write_frame(
                    stream,
                    &build_error_response(&format!("snapshot write failed: {e}")),
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

/// Write `data` to `path` atomically: write to a sibling tmp file then
/// `rename` over the target. `rename` is atomic on POSIX, so concurrent
/// readers either see the old content or the new content, never a
/// half-written file.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let pid = process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!(".spg-tmp-{pid}-{nanos}"));
    fs::write(&tmp, data)?;
    if let Err(e) = fs::rename(&tmp, path) {
        // Best-effort cleanup on rename failure.
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
        Ok(QueryResult::CommandOk { affected }) => {
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
