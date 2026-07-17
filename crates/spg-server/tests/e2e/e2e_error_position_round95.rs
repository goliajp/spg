//! v7.39 (read01 round 95) — syntax-error position (the ErrorResponse `P`
//! field). PG reports a 1-based character offset for a syntax error; psql
//! renders it as `LINE n: … ^`. SPG's wire never sent `P`, so psql showed the
//! message with no caret. The parser now carries the failing token's byte
//! offset (via new lexer per-token offsets), mapped to a 1-based char
//! position, and the wire attaches it as `P`.
//!
//! Positions locked against live PG 18.4 (its psql caret column).
//! Semantic-error positions (column-not-found, type mismatch) still have no
//! `P` — that needs analyzer/eval plumbing and is deferred.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-errpos-{label}-{nanos}"));
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
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

fn field(body: &[u8], code: u8) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let c = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if c == code {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        p = end + 1;
    }
    None
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

/// Send `sql`, return the (SQLSTATE, position) from its ErrorResponse.
fn err_pos(s: &mut TcpStream, sql: &str) -> (Option<String>, Option<String>) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("no error for {sql}"));
    (field(&e.body, b'C'), field(&e.body, b'P'))
}

#[test]
fn syntax_error_carries_pg_position() {
    let dir = unique_tmpdir("pos");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // Unexpected token: caret sits at the token (PG points at "FORM").
    let (c, p) = err_pos(&mut s, "SELECT * FORM t");
    assert_eq!(c.as_deref(), Some("42601"));
    assert_eq!(p.as_deref(), Some("10"));

    // Unexpected token mid-list: PG points at "3".
    let (_, p) = err_pos(&mut s, "SELECT 1, 2 3, 4");
    assert_eq!(p.as_deref(), Some("13"));

    // End of input: PG points one past the last char.
    let (_, p) = err_pos(&mut s, "SELECT 1 +");
    assert_eq!(p.as_deref(), Some("11"));

    let (_, p) = err_pos(&mut s, "SELECT * FROM t WHERE");
    assert_eq!(p.as_deref(), Some("22"));
}

#[test]
fn valid_statement_has_no_error() {
    let dir = unique_tmpdir("ok");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SELECT 1");
    let msgs = read_until_ready(&mut s);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "unexpected error for SELECT 1"
    );
}
