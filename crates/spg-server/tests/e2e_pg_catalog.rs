#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.6 PG-wire pg_catalog subset e2e.
//!
//! Verifies that the PG-wire shim synthesizes responses for common
//! pg_catalog probes — enough that a basic PG client doesn't
//! immediately bail when browsing.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-pgcat-{nanos}"));
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

/// Open mode (no admin password) → no auth phase.
fn connect_open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    // AuthOk + ParameterStatus(s) + BackendKeyData + ReadyForQuery
    let _ = read_until_ready(&mut s);
    s
}

/// Count DataRow + DataRowBatch messages until ReadyForQuery.
fn run_query_count_rows(s: &mut TcpStream, sql: &str) -> (Vec<PgMessage>, usize) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let n = msgs.iter().filter(|m| m.ty == b'D').count();
    (msgs, n)
}

#[test]
fn pg_class_returns_user_tables() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = wait_for_listener(&pg, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);

    // Seed two tables via the PG-wire native simple-query path.
    send_query(&mut s, "CREATE TABLE alpha (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "CREATE TABLE beta (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);

    // pg_class should now show two rows.
    let (msgs, n) = run_query_count_rows(&mut s, "SELECT relname FROM pg_catalog.pg_class");
    assert!(
        msgs.iter().any(|m| m.ty == b'T'),
        "expected RowDescription in pg_class response"
    );
    assert_eq!(n, 2, "expected 2 user tables in pg_class");
}

#[test]
fn pg_namespace_returns_public() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = connect_open(&pg);

    let (_msgs, n) = run_query_count_rows(&mut s, "SELECT * FROM pg_catalog.pg_namespace");
    assert_eq!(n, 1, "expected single 'public' namespace row");
}

#[test]
fn pg_database_returns_spg() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = connect_open(&pg);

    let (_msgs, n) = run_query_count_rows(&mut s, "SELECT * FROM pg_database");
    assert_eq!(n, 1, "expected single 'spg' database row");
}

#[test]
fn current_database_returns_text() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = connect_open(&pg);

    let (msgs, n) = run_query_count_rows(&mut s, "SELECT current_database()");
    assert_eq!(n, 1);
    // Find the DataRow and check it contains "spg"
    let dr = msgs.iter().find(|m| m.ty == b'D').unwrap();
    let payload = &dr.body;
    // [u16 col_count][i32 len][bytes...]
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1);
    let len = i32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    let s = std::str::from_utf8(&payload[6..6 + len as usize]).unwrap();
    assert_eq!(s, "spg");
}

#[test]
fn pg_tables_returns_two_rows() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let mut child = ChildGuard(spawn_server(&native, &pg, &db));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = connect_open(&pg);
    send_query(&mut s, "CREATE TABLE t1 (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "CREATE TABLE t2 (id INT NOT NULL)");
    let _ = read_until_ready(&mut s);

    let (_msgs, n) = run_query_count_rows(&mut s, "SELECT * FROM pg_catalog.pg_tables");
    assert_eq!(n, 2);
}
