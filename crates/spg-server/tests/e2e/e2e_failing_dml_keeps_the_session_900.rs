//! 9.0.0 — a DML that errors leaves the session alive, on both protocols.
//!
//! The commit queue's audit barrier asserted that every queued DML had
//! audited. The leader audits a statement that CHANGED durable state, so
//! a statement that errored — which changed nothing — failed the assert
//! and took the connection down with it:
//!
//! ```text
//!   INSERT INTO nosuchtable VALUES (1)
//!     PG 18.6   ERROR: relation "nosuchtable" does not exist
//!     SPG       connection to server was lost
//! ```
//!
//! Only in a debug build — the assert compiles out of a release one,
//! which is why no shipped binary did this, and why the debug test suite
//! had no coverage of a failing DML at all. This file is that coverage.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0admin\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    read_until_ready(&mut s);
    s
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

/// Send `sql` on the simple protocol; return its ErrorResponse message.
fn simple_error(s: &mut TcpStream, sql: &str) -> String {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let msgs = read_until_ready(s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: expected an error"));
    field(&e.body, b'M').unwrap_or_default()
}

/// Parse + Bind + Execute + Sync; return its ErrorResponse message.
fn extended_error(s: &mut TcpStream, sql: &str) -> String {
    let mut parse = vec![0];
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&0_u16.to_be_bytes());
    send_msg(s, b'P', &parse);
    let mut bind = vec![0, 0];
    bind.extend_from_slice(&0_u16.to_be_bytes());
    bind.extend_from_slice(&0_u16.to_be_bytes());
    bind.extend_from_slice(&0_u16.to_be_bytes());
    send_msg(s, b'B', &bind);
    let mut exec = vec![0];
    exec.extend_from_slice(&0_u32.to_be_bytes());
    send_msg(s, b'E', &exec);
    send_msg(s, b'S', &[]);
    let msgs = read_until_ready(s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: expected an error"));
    field(&e.body, b'M').unwrap_or_default()
}

/// Run `sql` on the simple protocol and require it to succeed.
fn run_simple(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let msgs = read_until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
}

/// The connection answers afterwards, which is the point.
fn still_answers(s: &mut TcpStream) -> bool {
    let mut body = Vec::new();
    body.extend_from_slice(b"SELECT 1\0");
    send_msg(s, b'Q', &body);
    read_until_ready(s).iter().any(|m| m.ty == b'D')
}

const FAILING: &[&str] = &[
    "INSERT INTO nosuchtable VALUES (1)",
    "UPDATE nosuchtable SET a = 1",
    "DELETE FROM nosuchtable",
];

#[test]
fn a_failing_dml_answers_and_the_session_lives_on_the_simple_protocol() {
    let dir =
        crate::common::tmp_base().join(format!("spg-e2e-faildml-simple-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The audit log arms the barrier and the WAL is what routes an
    // autocommit DML through the commit queue that holds it. A server
    // without both cannot reach this defect; the published image has
    // both.
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .arg_path(&dir.join("audit"))
        .arg_path(&dir.join("wal"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    for sql in FAILING {
        // PG 18.6: relation "nosuchtable" does not exist
        let msg = simple_error(&mut s, sql);
        assert!(
            msg.contains("relation \"nosuchtable\" does not exist"),
            "{sql}: {msg}"
        );
        assert!(still_answers(&mut s), "{sql}: session died");
    }

    run_simple(&mut s, "CREATE TABLE dup(id int PRIMARY KEY)");
    run_simple(&mut s, "INSERT INTO dup VALUES (1)");
    let msg = simple_error(&mut s, "INSERT INTO dup VALUES (1)");
    assert!(
        msg.contains("duplicate key") || msg.contains("unique"),
        "duplicate insert: {msg}"
    );
    assert!(still_answers(&mut s), "duplicate insert: session died");
}

#[test]
fn a_failing_dml_answers_and_the_session_lives_on_the_extended_protocol() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-faildml-ext-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The audit log arms the barrier and the WAL is what routes an
    // autocommit DML through the commit queue that holds it. A server
    // without both cannot reach this defect; the published image has
    // both.
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .arg_path(&dir.join("audit"))
        .arg_path(&dir.join("wal"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // A missing relation is refused at Parse on this protocol, which
    // never reaches the queue. A statement that parses and describes and
    // then fails at execution does — a duplicate key is the plainest.
    run_simple(&mut s, "CREATE TABLE dup(id int PRIMARY KEY)");
    run_simple(&mut s, "INSERT INTO dup VALUES (1)");
    for _ in 0..2 {
        let msg = extended_error(&mut s, "INSERT INTO dup VALUES (1)");
        assert!(
            msg.contains("duplicate key") || msg.contains("unique"),
            "duplicate insert: {msg}"
        );
        assert!(still_answers(&mut s), "duplicate insert: session died");
    }
}
