//! v4.7 PG-wire extended-query protocol e2e.
//!
//! Hand-rolls the Parse → Bind → Execute → Sync pipeline that
//! every modern PG driver (JDBC, asyncpg, psycopg3, ...) uses by
//! default. Verifies:
//! - parameterless prepared statement round-trips
//! - $1 / $2 text-format parameter substitution
//! - Close + reuse of statement names

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-pgext-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
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

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_parse(s: &mut TcpStream, name: &str, sql: &str) {
    let mut body = Vec::with_capacity(name.len() + sql.len() + 8);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes()); // 0 declared param types
    send_msg(s, b'P', &body);
}

fn send_bind_text(s: &mut TcpStream, portal: &str, stmt: &str, params: &[&str]) {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(stmt.as_bytes());
    body.push(0);
    // 0 format codes = all text by default
    body.extend_from_slice(&0u16.to_be_bytes());
    // n param values
    body.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        body.extend_from_slice(p.as_bytes());
    }
    // 0 result format codes = all text
    body.extend_from_slice(&0u16.to_be_bytes());
    send_msg(s, b'B', &body);
}

fn send_execute(s: &mut TcpStream, portal: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i32.to_be_bytes()); // 0 rows = unlimited
    send_msg(s, b'E', &body);
}

fn send_sync(s: &mut TcpStream) {
    send_msg(s, b'S', &[]);
}

fn send_describe_portal(s: &mut TcpStream, portal: &str) {
    let mut body = Vec::new();
    body.push(b'P');
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    send_msg(s, b'D', &body);
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn parameterless_prepared_select_round_trips() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // Set up data via simple query
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (2)");
    let _ = read_until_ready(&mut s);

    // Extended-query pipeline. v7.39 — Execute no longer emits
    // RowDescription (that belongs to Describe, per protocol); a
    // Describe(portal) is pipelined in to keep asserting the T frame.
    send_parse(&mut s, "", "SELECT id FROM t");
    send_bind_text(&mut s, "", "", &[]);
    send_describe_portal(&mut s, "");
    send_execute(&mut s, "");
    send_sync(&mut s);

    let msgs = read_until_ready(&mut s);
    // Expected sequence: ParseComplete (1), BindComplete (2),
    // RowDescription (T, from Describe), DataRow (D)*2,
    // CommandComplete (C), ReadyForQuery (Z)
    let types: Vec<u8> = msgs.iter().map(|m| m.ty).collect();
    assert!(
        types.contains(&b'1'),
        "expected ParseComplete, got {types:?}"
    );
    assert!(
        types.contains(&b'2'),
        "expected BindComplete, got {types:?}"
    );
    assert!(
        types.contains(&b'T'),
        "expected RowDescription, got {types:?}"
    );
    let drs = types.iter().filter(|&&t| t == b'D').count();
    assert_eq!(drs, 2, "expected 2 DataRows, got {drs}");
    assert!(
        types.contains(&b'C'),
        "expected CommandComplete, got {types:?}"
    );
}

#[test]
fn parameter_substitution_text_format() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)",
    );
    let _ = read_until_ready(&mut s);
    for i in 1..=3 {
        send_query(&mut s, &format!("INSERT INTO t VALUES ({i}, 'r-{i}')"));
        let _ = read_until_ready(&mut s);
    }

    // Prepare SELECT with $1 parameter.
    send_parse(&mut s, "by_id", "SELECT label FROM t WHERE id = $1");
    send_bind_text(&mut s, "p1", "by_id", &["2"]);
    send_execute(&mut s, "p1");
    send_sync(&mut s);
    let msgs = read_until_ready(&mut s);
    let drs: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert_eq!(drs.len(), 1, "expected one row for id=2");
    // DataRow body: [u16 col_count][i32 len][bytes...]
    let body = &drs[0].body;
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    let val = std::str::from_utf8(&body[6..6 + len as usize]).unwrap();
    assert_eq!(val, "r-2");
}

#[test]
fn dml_via_extended_query() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);

    // Parametrized INSERT via Parse/Bind/Execute
    send_parse(&mut s, "ins", "INSERT INTO t VALUES ($1)");
    send_bind_text(&mut s, "", "ins", &["42"]);
    send_execute(&mut s, "");
    send_sync(&mut s);
    let msgs = read_until_ready(&mut s);
    let types: Vec<u8> = msgs.iter().map(|m| m.ty).collect();
    assert!(
        types.contains(&b'C'),
        "expected CommandComplete, got {types:?}"
    );

    // Verify the row landed via simple query.
    send_query(&mut s, "SELECT id FROM t");
    let msgs = read_until_ready(&mut s);
    let drs = msgs.iter().filter(|m| m.ty == b'D').count();
    assert_eq!(drs, 1, "expected one row after parametrized INSERT");
}

// ── v7.39 (cursors) — Execute max_rows + PortalSuspended ──

fn send_execute_max(s: &mut TcpStream, portal: &str, max_rows: i32) {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    send_msg(s, b'E', &body);
}

#[test]
fn execute_max_rows_suspends_and_resumes_portal() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _guard = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    // 5-row table through the extended protocol.
    send_parse(&mut s, "", "CREATE TABLE cur (v INT)");
    send_bind_text(&mut s, "", "", &[]);
    send_execute(&mut s, "");
    send_sync(&mut s);
    read_until_ready(&mut s);
    send_parse(&mut s, "", "INSERT INTO cur VALUES (1),(2),(3),(4),(5)");
    send_bind_text(&mut s, "", "", &[]);
    send_execute(&mut s, "");
    send_sync(&mut s);
    read_until_ready(&mut s);

    // Fetch in batches of 2: 2 rows + 's', 2 rows + 's', 1 row + 'C'.
    send_parse(&mut s, "cst", "SELECT v FROM cur ORDER BY v");
    send_bind_text(&mut s, "cpt", "cst", &[]);
    send_execute_max(&mut s, "cpt", 2);
    send_execute_max(&mut s, "cpt", 2);
    send_execute_max(&mut s, "cpt", 2);
    send_sync(&mut s);

    let mut rows = 0usize;
    let mut suspends = 0usize;
    let mut completes = 0usize;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => rows += 1,
            b's' => suspends += 1,
            b'C' => completes += 1,
            b'E' => panic!("unexpected error: {}", String::from_utf8_lossy(&m.body)),
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(rows, 5, "all rows across the three Executes");
    assert_eq!(suspends, 2, "first two batches suspend the portal");
    assert_eq!(completes, 1, "only the final Execute completes (SELECT 5)");
}

// ── v7.39 (binary results) — Bind result-format=binary honoured ──

fn send_bind_binary_results(s: &mut TcpStream, portal: &str, stmt: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(stmt.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes()); // no param formats
    body.extend_from_slice(&0u16.to_be_bytes()); // no params
    // ONE result-format code = binary for every column.
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&1i16.to_be_bytes());
    send_msg(s, b'B', &body);
}

/// Split a DataRow body into per-cell payloads (None = SQL NULL).
fn cells(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let n = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut cur = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = i32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]);
        cur += 4;
        if len < 0 {
            out.push(None);
        } else {
            out.push(Some(body[cur..cur + len as usize].to_vec()));
            cur += len as usize;
        }
    }
    out
}

#[test]
fn binary_result_format_encodes_pg_binary() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _guard = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_parse(
        &mut s,
        "b1",
        "SELECT 7, 300000000000, true, 'hi', 1.5::float8, \
         '2024-03-15'::date, '2024-03-15 12:00:00'::timestamp, \
         12345.678::numeric(10,3), NULL::int",
    );
    send_bind_binary_results(&mut s, "bp1", "b1");
    send_execute(&mut s, "bp1");
    send_sync(&mut s);

    let mut row: Option<Vec<Option<Vec<u8>>>> = None;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => row = Some(cells(&m.body)),
            b'E' => panic!("error: {}", String::from_utf8_lossy(&m.body)),
            b'Z' => break,
            _ => {}
        }
    }
    let row = row.expect("one data row");
    // int4 7 — 4-byte BE.
    assert_eq!(row[0].as_deref(), Some(&7i32.to_be_bytes()[..]));
    // int8 — 8-byte BE.
    assert_eq!(row[1].as_deref(), Some(&300000000000i64.to_be_bytes()[..]));
    // bool true — single 0x01.
    assert_eq!(row[2].as_deref(), Some(&[1u8][..]));
    // text — raw UTF-8.
    assert_eq!(row[3].as_deref(), Some(&b"hi"[..]));
    // float8 — IEEE BE.
    assert_eq!(row[4].as_deref(), Some(&1.5f64.to_be_bytes()[..]));
    // date — days since 2000-01-01: 2024-03-15 = 8840.
    assert_eq!(row[5].as_deref(), Some(&8840i32.to_be_bytes()[..]));
    // timestamp — micros since 2000-01-01 midnight.
    let expect_ts: i64 = (8840i64 * 86_400 + 12 * 3600) * 1_000_000;
    assert_eq!(row[6].as_deref(), Some(&expect_ts.to_be_bytes()[..]));
    // numeric 12345.678 — ndigits=3, weight=1, sign=0, dscale=3,
    // digits [1, 2345, 6780] (base 10000).
    let num = row[7].as_deref().expect("numeric non-null");
    let words: Vec<u16> = num
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(words, vec![3, 1, 0, 3, 1, 2345, 6780], "numeric wire words");
    // NULL — len -1 (decoded as None).
    assert_eq!(row[8], None);
}
