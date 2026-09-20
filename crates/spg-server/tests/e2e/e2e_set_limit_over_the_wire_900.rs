//! 9.0.0 — `set_limit()` did not stick over the wire.
//!
//! The engine pin passes: `Engine::execute` runs the statement-level
//! pass that folds a state-changing call and writes the setting. The
//! SERVER does not always get there — a `SELECT` whose text carries no
//! known mutating call takes a read-only fast path that holds `&self`,
//! and that path cannot run the pass at all.
//!
//! So `SELECT set_limit(0.66)` answered `0.66` and wrote nothing; the
//! next `SELECT show_limit()` said `0.3`. It only appeared to work when
//! psql happened to put several statements in one message, because a
//! batch does not take the fast path.
//!
//! The list of names that force the write path is hand-kept, and this
//! is the third family to have been left out of it (advisory locks and
//! the large objects were the first two, each with its own round).

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
fn set_limit_sticks_when_it_is_the_whole_statement() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-setlimit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // A DDL answers CommandComplete, not a row.
    let msgs = query(&mut s, "CREATE EXTENSION pg_trgm");
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "CREATE EXTENSION pg_trgm was refused"
    );
    // The measured default, and the value the operator below compares to.
    assert_eq!(first_value(&mut s, "SELECT show_limit()"), "0.3");
    // Each of these is a message of its own — the shape that lost the write.
    assert_eq!(first_value(&mut s, "SELECT set_limit(0.66)"), "0.66");
    assert_eq!(first_value(&mut s, "SELECT show_limit()"), "0.66");
    // And the operator reads what was written, over the wire too.
    assert_eq!(first_value(&mut s, "SELECT 'cat' % 'cot'"), "f");
    assert_eq!(first_value(&mut s, "SELECT set_limit(0.05)"), "0.05");
    assert_eq!(first_value(&mut s, "SELECT 'cat' % 'cot'"), "t");
}
