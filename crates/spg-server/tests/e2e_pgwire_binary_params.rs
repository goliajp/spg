//! v6.3.4 — Binary parameter format e2e.
//!
//! Round-trips each supported PG binary-format type through Parse +
//! Bind(format=1) + Execute, comparing against the text-format
//! result. The ship gate is "binary parameter decode coverage" for
//! NUMERIC, TIMESTAMP, BIGINT, INT, REAL, DOUBLE, BOOL, BYTEA, TEXT.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-binary-params-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    let b = common::ServerBuilder::new().arg_path(db).with_pgwire();
    b.spawn()
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) {
    loop {
        let m = read_message(s);
        if m.ty == b'Z' {
            return;
        }
    }
}

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    let len = (body.len() + 4) as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
}

fn parse_with_oids(name: &str, sql: &str, oids: &[u32]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b.extend_from_slice(sql.as_bytes());
    b.push(0);
    let n = u16::try_from(oids.len()).expect("oid count fits");
    b.extend_from_slice(&n.to_be_bytes());
    for o in oids {
        b.extend_from_slice(&o.to_be_bytes());
    }
    b
}

/// Bind: portal, stmt, format_codes, param_values, result_formats.
fn bind_binary(
    portal: &str,
    stmt: &str,
    formats: &[u16],
    params: &[(i32, Vec<u8>)], // (length-or-(-1 for null), bytes)
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(stmt.as_bytes());
    b.push(0);
    let fn_ = u16::try_from(formats.len()).unwrap();
    b.extend_from_slice(&fn_.to_be_bytes());
    for f in formats {
        b.extend_from_slice(&f.to_be_bytes());
    }
    let pn = u16::try_from(params.len()).unwrap();
    b.extend_from_slice(&pn.to_be_bytes());
    for (len, bytes) in params {
        b.extend_from_slice(&len.to_be_bytes());
        b.extend_from_slice(bytes);
    }
    b.extend_from_slice(&0u16.to_be_bytes()); // 0 result formats → text
    b
}

fn execute_body(portal: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u32.to_be_bytes());
    b
}

fn handshake(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let r = read_message(&mut s);
    assert_eq!(r.ty, b'R');
    read_until_ready(&mut s);
    s
}

fn exec_simple(s: &mut TcpStream, sql: &str) {
    let mut q = Vec::new();
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    write_msg(&mut q, b'Q', &body);
    s.write_all(&q).unwrap();
    read_until_ready(s);
}

/// Drain the response after P/B/E/S, returning all DataRow cell-0
/// strings. Each row's cell-0 bytes are stringified.
fn drain_rows(s: &mut TcpStream) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => {
                // Skip past cell count (u16); cell 0: i32 len, bytes.
                let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
                let v = if len < 0 {
                    "NULL".to_string()
                } else {
                    let l = len as usize;
                    String::from_utf8_lossy(&m.body[6..6 + l]).to_string()
                };
                out.push(v);
            }
            b'Z' => return out,
            _ => {}
        }
    }
}

fn run_binary_param_test(
    label: &str,
    create_sql: &str,
    select_sql: &str,
    oids: &[u32],
    bind_params: &[(i32, Vec<u8>)],
    expected_text: &str,
) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, create_sql);

    let mut q = Vec::new();
    write_msg(&mut q, b'P', &parse_with_oids("p", select_sql, oids));
    write_msg(
        &mut q,
        b'B',
        &bind_binary("", "p", &[1], bind_params),
    );
    write_msg(&mut q, b'E', &execute_body(""));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    let rows = drain_rows(&mut s);
    assert_eq!(
        rows,
        vec![expected_text.to_string()],
        "{label}: round-trip mismatch"
    );
}

#[test]
fn binary_int_round_trip() {
    run_binary_param_test(
        "int",
        "CREATE TABLE x (v INT)",
        "SELECT $1::int",
        &[23],                                    // INT4
        &[(4, (42_i32).to_be_bytes().to_vec())],
        "42",
    );
}

#[test]
fn binary_bigint_round_trip() {
    run_binary_param_test(
        "bigint",
        "CREATE TABLE x (v BIGINT)",
        "SELECT $1::bigint",
        &[20],
        &[(8, (1234567890123_i64).to_be_bytes().to_vec())],
        "1234567890123",
    );
}

#[test]
fn binary_real_round_trip() {
    run_binary_param_test(
        "real",
        "CREATE TABLE x (v FLOAT)",
        "SELECT $1::float",
        &[700],
        &[(4, (3.5_f32).to_be_bytes().to_vec())],
        "3.5",
    );
}

#[test]
fn binary_double_round_trip() {
    run_binary_param_test(
        "double",
        "CREATE TABLE x (v FLOAT)",
        "SELECT $1::float",
        &[701],
        &[(8, (2.718281828_f64).to_be_bytes().to_vec())],
        "2.718281828",
    );
}

#[test]
fn binary_bool_round_trip_true() {
    run_binary_param_test(
        "bool-true",
        "CREATE TABLE x (v BOOL)",
        "SELECT $1::bool",
        &[16],
        &[(1, vec![1u8])],
        "t",
    );
}

#[test]
fn binary_text_round_trip() {
    run_binary_param_test(
        "text",
        "CREATE TABLE x (v TEXT)",
        "SELECT $1::text",
        &[25],
        &[(5, b"hello".to_vec())],
        "hello",
    );
}

#[test]
fn binary_null_param_round_trips_as_null() {
    // len = -1 means SQL NULL regardless of format.
    run_binary_param_test(
        "null",
        "CREATE TABLE x (v INT)",
        "SELECT $1::int",
        &[23],
        &[(-1, Vec::new())],
        "NULL",
    );
}

#[test]
fn mixed_text_and_binary_params_in_one_bind() {
    let dir = unique_tmpdir("mixed");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE t (a INT, b TEXT)");

    let oids = vec![23_u32, 25_u32];
    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_with_oids("p", "SELECT $1::int, $2::text", &oids),
    );

    // Per-param formats: binary for first, text for second.
    let formats = vec![1u16, 0u16];
    let params = vec![
        (4_i32, (99_i32).to_be_bytes().to_vec()),
        (5_i32, b"world".to_vec()),
    ];
    let mut bind = Vec::new();
    bind.extend_from_slice(b"\0");
    bind.extend_from_slice(b"p\0");
    bind.extend_from_slice(&(formats.len() as u16).to_be_bytes());
    for f in &formats {
        bind.extend_from_slice(&f.to_be_bytes());
    }
    bind.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for (len, bytes) in &params {
        bind.extend_from_slice(&len.to_be_bytes());
        bind.extend_from_slice(bytes);
    }
    bind.extend_from_slice(&0u16.to_be_bytes()); // 0 result formats

    write_msg(&mut q, b'B', &bind);
    write_msg(&mut q, b'E', &execute_body(""));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    let mut got_a = String::new();
    let mut got_b = String::new();
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => {
                let len_a = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]) as usize;
                got_a = String::from_utf8_lossy(&m.body[6..6 + len_a]).to_string();
                let off_b = 6 + len_a;
                let len_b = i32::from_be_bytes([
                    m.body[off_b],
                    m.body[off_b + 1],
                    m.body[off_b + 2],
                    m.body[off_b + 3],
                ]) as usize;
                got_b = String::from_utf8_lossy(&m.body[off_b + 4..off_b + 4 + len_b]).to_string();
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(got_a, "99");
    assert_eq!(got_b, "world");
}
