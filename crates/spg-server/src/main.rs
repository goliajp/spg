//! SPG daemon — TCP listener that accepts wire frames and dispatches them.
//!
//! v0.5 supports two opcodes:
//! - `Ping` → replies `Pong`.
//! - `Query` → locks the shared `Engine`, runs the SQL, emits a result-frame
//!   chain (`RowDescription` + `DataRow`* + `CommandComplete`) or a single
//!   `ErrorResponse` on failure.
//!
//! The engine is shared across connections via `Arc<Mutex<…>>`. v0.5 keeps
//! the locking coarse-grained (one writer at a time) — matches the Q1
//! decision from the L1 plan (single-writer + multi-reader; v0.5 simplifies
//! the read side to also acquire the same lock).

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::{ColumnSchema, DataType, Row, Value};
use spg_wire::{
    ColumnDesc, Frame, FrameError, Op, WireType, WireValue, build_command_complete, build_data_row,
    build_error_response, build_row_description, decode, encode, parse_query,
};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_CHUNK: usize = 4096;

fn main() {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());
    if let Err(e) = run(&addr) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

fn run(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("spg-server: listening on {local}");

    let engine = Arc::new(Mutex::new(Engine::new()));

    for stream in listener.incoming() {
        let stream = stream?;
        let engine = Arc::clone(&engine);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(stream, &engine) {
                eprintln!("spg-server: conn {peer:?}: {e}");
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, engine: &Mutex<Engine>) -> std::io::Result<()> {
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
                    dispatch(&mut stream, &frame, engine)?;
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

fn dispatch(stream: &mut TcpStream, frame: &Frame, engine: &Mutex<Engine>) -> std::io::Result<()> {
    match frame.op {
        Op::Ping => write_frame(stream, &Frame::pong()),
        Op::Query => {
            let sql = match parse_query(frame) {
                Ok(s) => s.to_string(),
                Err(e) => return write_frame(stream, &build_error_response(&e.to_string())),
            };
            let result = engine
                .lock()
                .map_err(|_| std::io::Error::other("engine mutex poisoned"))?
                .execute(&sql);
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
            // Use rows.len() as the "affected" count for SELECTs — useful to
            // the client as a row count even though SQL itself doesn't have a
            // standard meaning here.
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
