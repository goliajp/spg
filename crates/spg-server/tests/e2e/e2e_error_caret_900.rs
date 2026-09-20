//! 9.0.0 — SPG drew a caret PostgreSQL does not draw.
//!
//! The host locates an error's subject by reading the first quoted word
//! out of the message and finding it in the statement. That is right for
//! the four families PostgreSQL positions, and wrong for everything
//! else: measured on 18.6, `cannot insert a non-DEFAULT value into
//! column "id"` and `column "a" of relation "t" is not an identity
//! column` carry no position at all, and SPG pointed at the column in
//! both.
//!
//! The locator takes an allow-list now, so a message added later has to
//! ask for a caret rather than getting one because it happens to quote
//! a word.

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

/// One statement, one message — the shape that took the fast path.
fn query(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    read_until_ready(s)
}

/// The first column of the first `DataRow`, as text.
fn first_value(s: &mut TcpStream, sql: &str) -> String {
    let msgs = query(s, sql);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
    let d = msgs
        .iter()
        .find(|m| m.ty == b'D')
        .unwrap_or_else(|| panic!("{sql}: no DataRow"));
    // int16 column count, then int32 length + bytes per column.
    let len = i32::from_be_bytes([d.body[2], d.body[3], d.body[4], d.body[5]]);
    if len < 0 {
        return String::from("NULL");
    }
    let n = len as usize;
    String::from_utf8_lossy(&d.body[6..6 + n]).into_owned()
}

#[test]
fn only_the_families_postgresql_positions_get_a_caret() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-caret-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    let msgs = query(
        &mut s,
        "CREATE TABLE cz (id int GENERATED ALWAYS AS IDENTITY, a int, b int)",
    );
    assert!(msgs.iter().all(|m| m.ty != b'E'), "CREATE TABLE refused");

    // No position, measured on PG 18.6.
    for sql in [
        "INSERT INTO cz(id, a) VALUES (1, 1)",
        "ALTER TABLE cz ALTER COLUMN a DROP IDENTITY",
    ] {
        assert_eq!(position_of(&mut s, sql), None, "{sql}");
    }
    // A position, measured on PG 18.6 — the four families the locator
    // still serves.
    for sql in [
        "SELECT nosuch FROM cz",
        "SELECT * FROM cz WHERE x.a = 1",
        "SELECT * FROM nosuchtable",
        "SELECT a FROM cz GROUP BY b",
    ] {
        assert!(position_of(&mut s, sql).is_some(), "{sql}");
    }
}

/// The `P` field of the ErrorResponse `sql` raises, if it carries one.
fn position_of(s: &mut TcpStream, sql: &str) -> Option<String> {
    let msgs = query(s, sql);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: answered instead of refusing"));
    let body = &e.body;
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let code = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if code == b'P' {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        p = end + 1;
    }
    None
}
