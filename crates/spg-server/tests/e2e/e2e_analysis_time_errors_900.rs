//! 9.0.0 — the semantic errors PostgreSQL raises before it scans, over
//! the wire.
//!
//! The read path a client takes is not the one an in-process `Engine`
//! takes: an uncorrelated scalar subquery is resolved by EXECUTING it
//! there, and that execution runs the inner statement's own column
//! check. So an engine pin passes with the analysis pass removed, and
//! the server answers zero rows — which is what it did.
//!
//! ```text
//!                                         PG 18.6                                SPG 8.0.4
//!   SELECT CAST(id AS nosuchtype) FROM t  type "nosuchtype" does not exist       0 rows
//!   SELECT * FROM t WHERE n               argument of WHERE must be type boolean 0 rows
//!   SELECT (SELECT s.nosuch FROM t s)     column s.nosuch does not exist         0 rows
//! ```
//!
//! The table is EMPTY in every case — that is the whole point.

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

fn query(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    read_until_ready(s)
}

fn run(s: &mut TcpStream, sql: &str) {
    let msgs = query(s, sql);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
}

/// The message `sql` is refused with.
fn refused(s: &mut TcpStream, sql: &str) -> String {
    let msgs = query(s, sql);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: answered instead of refusing"));
    field(&e.body, b'M').unwrap_or_default()
}

#[test]
fn an_empty_table_still_refuses_what_cannot_mean_anything() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-analysis-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run(&mut s, "CREATE TABLE a9(id int, name text, n numeric)");

    for (sql, want) in [
        (
            "SELECT CAST(id AS nosuchtype) FROM a9",
            "type \"nosuchtype\" does not exist",
        ),
        (
            "SELECT * FROM a9 WHERE n",
            "argument of WHERE must be type boolean, not type numeric",
        ),
        (
            "SELECT (SELECT s.nosuch FROM a9 s) FROM a9",
            "column s.nosuch does not exist",
        ),
    ] {
        let m = refused(&mut s, sql);
        assert!(m.contains(want), "{sql}: {m}");
    }

    // And the statements PostgreSQL accepts are still accepted.
    for sql in [
        "SELECT id::text FROM a9",
        "SELECT * FROM a9 WHERE id = 1",
        "SELECT * FROM a9 WHERE 't'",
        "SELECT (SELECT o.id FROM a9 s) FROM a9 o",
    ] {
        let msgs = query(&mut s, sql);
        assert!(
            msgs.iter().all(|m| m.ty != b'E'),
            "{sql}: refused a statement PostgreSQL accepts"
        );
    }
}
