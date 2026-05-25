//! SPG CLI.
//!
//! Subcommands:
//! - `spg ping [addr]`               — sanity check the daemon is reachable.
//! - `spg query <sql> [addr]`        — send SQL, print the result or error.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process;
use std::time::Duration;

use spg_wire::{
    ColumnDesc, Frame, FrameError, Op, WireValue, build_auth, build_query, build_stats_request,
    encode, parse_command_complete, parse_data_row, parse_error_response, parse_row_description,
    parse_stats_response,
};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("ping") => {
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match ping(&addr) {
                Ok(()) => println!("PONG"),
                Err(e) => die(&format!("ping failed: {e}"), 1),
            }
        }
        Some("query") => {
            let Some(sql) = args.next() else {
                die("usage: spg query <sql> [addr]", 2);
                return;
            };
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match query(&addr, &sql) {
                Ok(()) => {}
                Err(e) => die(&format!("query failed: {e}"), 1),
            }
        }
        Some("stats") => {
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match stats(&addr) {
                Ok(text) => print!("{text}"),
                Err(e) => die(&format!("stats failed: {e}"), 1),
            }
        }
        Some("version") => {
            println!("spg {}", env!("CARGO_PKG_VERSION"));
        }
        Some(other) => die(&format!("unknown command: {other}"), 2),
        None => die("usage: spg <ping|query|stats|version> ...", 2),
    }
}

/// Pull the password from `SPG_PASSWORD` (empty string treated as
/// "no password"). Returns `Ok(None)` when nothing is configured.
fn env_password() -> Option<String> {
    env::var("SPG_PASSWORD").ok().filter(|s| !s.is_empty())
}

/// Send `AUTH <password>` and consume the reply. No-op when no
/// password is configured — keeps the open-instance code path branchless
/// at every call site.
fn maybe_authenticate(stream: &mut TcpStream) -> Result<(), String> {
    let Some(pw) = env_password() else {
        return Ok(());
    };
    let mut out = Vec::new();
    encode(&build_auth(&pw), &mut out).map_err(|e| format!("encode AUTH: {e}"))?;
    stream
        .write_all(&out)
        .map_err(|e| format!("write AUTH: {e}"))?;
    let frame = read_one_frame(stream)?;
    match frame.op {
        Op::Pong => Ok(()),
        Op::ErrorResponse | Op::Error => {
            let msg =
                parse_error_response(&frame).map_or_else(|_| "<undecodable>".into(), str::to_owned);
            Err(format!("AUTH rejected: {msg}"))
        }
        other => Err(format!("unexpected AUTH reply op {other:?}")),
    }
}

fn stats(addr: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    maybe_authenticate(&mut stream)?;
    let mut out = Vec::new();
    encode(&build_stats_request(), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;
    let frame = read_one_frame(&mut stream)?;
    match frame.op {
        Op::StatsResponse => parse_stats_response(&frame)
            .map(str::to_owned)
            .map_err(|e| format!("decode: {e}")),
        Op::ErrorResponse | Op::Error => {
            let msg =
                parse_error_response(&frame).map_or_else(|_| "<undecodable>".into(), str::to_owned);
            Err(format!("server: {msg}"))
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn die(msg: &str, code: i32) {
    eprintln!("spg: {msg}");
    process::exit(code);
}

fn ping(addr: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    // Ping itself is always allowed unauthenticated; skip the AUTH
    // round-trip to keep `spg ping` a true low-overhead health check.
    let mut out = Vec::new();
    encode(&Frame::ping(), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;

    let frame = read_one_frame(&mut stream)?;
    match frame.op {
        Op::Pong => Ok(()),
        Op::Error | Op::ErrorResponse => {
            let msg = parse_error_response(&frame)
                .map(str::to_owned)
                .or_else(|_| {
                    Ok::<String, FrameError>(String::from_utf8_lossy(&frame.payload).into_owned())
                })
                .unwrap_or_else(|_| "<undecodable error>".into());
            Err(format!("server error: {msg}"))
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn query(addr: &str, sql: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    maybe_authenticate(&mut stream)?;
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;

    // First reply: either RowDescription (start of a row set), CommandComplete
    // (DDL/DML happy path), or ErrorResponse.
    let first = read_one_frame(&mut stream)?;
    match first.op {
        Op::CommandComplete => {
            let affected = parse_command_complete(&first).map_err(|e| format!("decode CC: {e}"))?;
            println!("OK ({affected} row(s) affected)");
            Ok(())
        }
        Op::ErrorResponse => {
            let msg = parse_error_response(&first).map_err(|e| format!("decode error: {e}"))?;
            Err(msg.into())
        }
        Op::RowDescription => {
            let cols = parse_row_description(&first).map_err(|e| format!("decode RD: {e}"))?;
            let mut rows: Vec<Vec<WireValue>> = Vec::new();
            loop {
                let f = read_one_frame(&mut stream)?;
                match f.op {
                    Op::DataRow => {
                        let row = parse_data_row(&f).map_err(|e| format!("decode DR: {e}"))?;
                        rows.push(row);
                    }
                    Op::CommandComplete => break,
                    Op::ErrorResponse => {
                        let msg =
                            parse_error_response(&f).map_err(|e| format!("decode error: {e}"))?;
                        return Err(msg.into());
                    }
                    other => return Err(format!("unexpected op in row stream: {other:?}")),
                }
            }
            print_table(&cols, &rows);
            Ok(())
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn read_one_frame(stream: &mut TcpStream) -> Result<Frame, String> {
    // Use exact-length reads so we never leave already-arrived bytes
    // stranded in a stack-local buffer between back-to-back frames
    // (which the server emits for SELECT: RowDescription + DataRow* + CC).
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .map_err(|e| format!("read header: {e}"))?;
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream
            .read_exact(&mut payload)
            .map_err(|e| format!("read payload: {e}"))?;
    }
    Ok(Frame { op, payload })
}

fn print_table(cols: &[ColumnDesc], rows: &[Vec<WireValue>]) {
    // Compute column widths from headers and stringified cell values.
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(format_value).collect())
        .collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| c.name.len()).collect();
    for row in &cells {
        for (i, s) in row.iter().enumerate() {
            if s.len() > widths[i] {
                widths[i] = s.len();
            }
        }
    }

    // Header
    let mut line = String::new();
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str(" | ");
        }
        line.push_str(&pad(&c.name, widths[i]));
    }
    println!("{line}");

    // Separator
    line.clear();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("-+-");
        }
        line.push_str(&"-".repeat(*w));
    }
    println!("{line}");

    // Rows
    for row in &cells {
        line.clear();
        for (i, s) in row.iter().enumerate() {
            if i > 0 {
                line.push_str(" | ");
            }
            line.push_str(&pad(s, widths[i]));
        }
        println!("{line}");
    }
    println!("({} row(s))", rows.len());
}

fn pad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.into()
    } else {
        let mut out = String::with_capacity(w);
        out.push_str(s);
        for _ in s.len()..w {
            out.push(' ');
        }
        out
    }
}

fn format_value(v: &WireValue) -> String {
    match v {
        WireValue::Null => "NULL".into(),
        WireValue::Int(n) => n.to_string(),
        WireValue::BigInt(n) => n.to_string(),
        WireValue::Float(x) => format!("{x}"),
        WireValue::Text(s) => s.clone(),
        WireValue::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).into(),
        WireValue::Vector(v) => {
            use core::fmt::Write as _;
            let mut s = String::from("[");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write!(s, "{x}").expect("format to String");
            }
            s.push(']');
            s
        }
    }
}
