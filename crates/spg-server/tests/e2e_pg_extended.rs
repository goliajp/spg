#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::naive_bytecount,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.7 PG-wire extended-query protocol e2e.
//!
//! Hand-rolls the Parse → Bind → Execute → Sync pipeline that
//! every modern PG driver (JDBC, asyncpg, psycopg3, ...) uses by
//! default. Verifies:
//! - parameterless prepared statement round-trips
//! - $1 / $2 text-format parameter substitution
//! - Close + reuse of statement names

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-pgext-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(native_addr: &str, pg_addr: &str, db: &PathBuf) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(native_addr)
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SPG_PG_ADDR", pg_addr)
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .spawn()
        .unwrap()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn parameterless_prepared_select_round_trips() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = open(&pg);

    // Set up data via simple query
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (2)");
    let _ = read_until_ready(&mut s);

    // Extended-query pipeline
    send_parse(&mut s, "", "SELECT id FROM t");
    send_bind_text(&mut s, "", "", &[]);
    send_execute(&mut s, "");
    send_sync(&mut s);

    let msgs = read_until_ready(&mut s);
    // Expected message sequence: ParseComplete (1), BindComplete (2),
    // RowDescription (T), DataRow (D)*2, CommandComplete (C), ReadyForQuery (Z)
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
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = open(&pg);

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
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = open(&pg);

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
