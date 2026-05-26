#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.3 PostgreSQL wire-protocol compatibility e2e.
//!
//! Hand-rolled PG client just enough to:
//! 1. Open a connection (StartupMessage + cleartext password)
//! 2. Run a Query
//! 3. Read RowDescription + DataRow* + CommandComplete + ReadyForQuery
//!
//! Confirms basic interoperability without pulling in a full PG
//! client library — the same skeleton psql / DBeaver / Metabase
//! drivers use.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-pgwire-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(native_addr: &str, pg_addr: &str, db: &PathBuf, admin_pw: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(native_addr)
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.env("SPG_PG_ADDR", pg_addr);
    cmd.env_remove("SPG_PASSWORD");
    if let Some(pw) = admin_pw {
        cmd.env("SPG_ADMIN_PASSWORD", pw);
    } else {
        cmd.env_remove("SPG_ADMIN_PASSWORD");
    }
    cmd.spawn().unwrap()
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

/// One PG message — typed messages have a 1-byte type, length (BE
/// u32 including itself), then body.
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
    body.extend_from_slice(&196608u32.to_be_bytes()); // protocol v3
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0); // terminator
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_password(s: &mut TcpStream, password: &str) {
    let mut body = Vec::with_capacity(password.len() + 1);
    body.extend_from_slice(password.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'p');
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

/// Walk through the post-auth handshake (ParameterStatus repeatedly,
/// BackendKeyData, ReadyForQuery). Returns when we see 'Z'.
fn read_until_ready(s: &mut TcpStream) {
    loop {
        let m = read_message(s);
        if m.ty == b'Z' {
            return;
        }
    }
}

// NOTE: the v4.3 cleartext-password tests previously here
// (psql_style_handshake_then_select, wrong_password_gets_error)
// were retired in v4.8 — bootstrap admin now has SCRAM-SHA-256
// secrets, so the server advertises SCRAM (AuthSASL subtype 10)
// not CleartextPassword. Equivalent coverage lives in
// tests/e2e_pg_scram.rs (full SCRAM handshake + wrong-creds
// rejection) and tests/e2e_pg_catalog.rs (open mode no-auth).

#[test]
fn select_version_canned_response_works() {
    let native = pick_free_addr();
    let pg = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    // No admin password = open mode; PG-wire skips auth.
    let mut child = ChildGuard(spawn_server(&native, &pg, &db, None));
    let _ = wait_for_listener(&native, &mut child.0);
    let mut s = wait_for_listener(&pg, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_startup(&mut s, "anyone");
    // Open mode: server skips straight to AuthOk + ParameterStatus + ReadyForQuery.
    let ok = read_message(&mut s);
    assert_eq!(ok.ty, b'R');
    read_until_ready(&mut s);

    send_query(&mut s, "SELECT version()");
    let rd = read_message(&mut s);
    assert_eq!(rd.ty, b'T');
    let dr = read_message(&mut s);
    assert_eq!(dr.ty, b'D');
    // The DataRow body starts with [u16 cell_count][i32 len][bytes…]
    let cell_count = u16::from_be_bytes([dr.body[0], dr.body[1]]);
    assert_eq!(cell_count, 1);
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    assert!(len > 0);
    let value = std::str::from_utf8(&dr.body[6..6 + len as usize]).unwrap();
    assert!(value.contains("spg"), "got {value:?}");
    let _ = read_message(&mut s); // C
    read_until_ready(&mut s);
}
