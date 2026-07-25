//! v6.3.3 — Describe statement pre-Execute e2e.
//!
//! Verifies that Parse → Describe('S', name) → Sync returns
//! ParameterDescription + RowDescription (or NoData) without
//! requiring an Execute roundtrip first.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-pgwire-describe-{label}-{nanos}"));
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

fn parse_msg_body(name: &str, sql: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(name.len() + sql.len() + 8);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b.extend_from_slice(sql.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes());
    b
}

fn describe_msg_body(kind: u8, name: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(name.len() + 2);
    b.push(kind);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b
}

fn handshake(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let ok = read_message(&mut s);
    assert_eq!(ok.ty, b'R');
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

#[test]
fn describe_statement_returns_row_description_for_simple_select() {
    let dir = unique_tmpdir("simple-select");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE t (id INT, name TEXT)");

    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_msg_body("s1", "SELECT id, name FROM t"),
    );
    write_msg(&mut q, b'D', &describe_msg_body(b'S', "s1"));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    // Expected stream: ParseComplete '1', ParameterDescription 't',
    // RowDescription 'T', ReadyForQuery 'Z'.
    let parse_complete = read_message(&mut s);
    assert_eq!(parse_complete.ty, b'1');
    let param_desc = read_message(&mut s);
    assert_eq!(param_desc.ty, b't', "expected ParameterDescription");
    // Empty param list: u16=0.
    assert_eq!(&param_desc.body[..2], &[0u8, 0u8]);
    let row_desc = read_message(&mut s);
    assert_eq!(row_desc.ty, b'T', "expected RowDescription");
    // u16 cell-count = 2.
    assert_eq!(u16::from_be_bytes([row_desc.body[0], row_desc.body[1]]), 2);
    let rfq = read_message(&mut s);
    assert_eq!(rfq.ty, b'Z');
}

#[test]
fn describe_statement_returns_param_oids_for_placeholders() {
    let dir = unique_tmpdir("placeholders");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE t (id INT, name TEXT)");

    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_msg_body("s2", "SELECT id FROM t WHERE id = $1 AND name = $2"),
    );
    write_msg(&mut q, b'D', &describe_msg_body(b'S', "s2"));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    let parse_complete = read_message(&mut s);
    assert_eq!(parse_complete.ty, b'1');
    let param_desc = read_message(&mut s);
    assert_eq!(param_desc.ty, b't');
    assert_eq!(
        u16::from_be_bytes([param_desc.body[0], param_desc.body[1]]),
        2,
        "expected 2 parameters"
    );
}

#[test]
fn describe_statement_returns_nodata_for_non_select() {
    let dir = unique_tmpdir("non-select");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE t (id INT)");

    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_msg_body("s3", "INSERT INTO t VALUES (1)"),
    );
    write_msg(&mut q, b'D', &describe_msg_body(b'S', "s3"));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    let parse_complete = read_message(&mut s);
    assert_eq!(parse_complete.ty, b'1');
    let param_desc = read_message(&mut s);
    assert_eq!(param_desc.ty, b't');
    let no_data = read_message(&mut s);
    assert_eq!(no_data.ty, b'n', "INSERT should report NoData");
}

#[test]
fn describe_statement_returns_row_description_for_join() {
    let dir = unique_tmpdir("join");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE a (id INT)");
    exec_simple(&mut s, "CREATE TABLE b (id INT)");

    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_msg_body("s4", "SELECT * FROM a JOIN b ON a.id = b.id"),
    );
    write_msg(&mut q, b'D', &describe_msg_body(b'S', "s4"));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();

    let parse_complete = read_message(&mut s);
    assert_eq!(parse_complete.ty, b'1');
    let param_desc = read_message(&mut s);
    assert_eq!(param_desc.ty, b't');
    let response = read_message(&mut s);
    // v7.39 (round 462) — this used to require NoData. Execute never
    // sends a RowDescription, so NoData meant a sqlx / JDBC client
    // received the join's rows with no column metadata at all and
    // `row.get(0)` was out of bounds. PG18 describes it; so must SPG.
    assert_eq!(
        response.ty, b'T',
        "a JOIN must declare its columns — Describe is the only place they are sent"
    );
    // Two `id` columns, one per side, named bare as PG18 names them.
    let n = u16::from_be_bytes([response.body[0], response.body[1]]);
    assert_eq!(n, 2, "join of two one-column tables declares two columns");
}
