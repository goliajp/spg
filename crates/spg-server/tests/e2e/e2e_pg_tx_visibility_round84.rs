//! v7.39 (read01 round 84) — read-your-own-writes over the PG wire.
//!
//! A differential isolation sweep against live PG18.4 found that, over the PG
//! wire, a statement inside an explicit `BEGIN … ` block could not see the
//! transaction's OWN prior writes:
//!
//!     BEGIN;
//!     INSERT INTO t VALUES (6, 60);
//!     SELECT count(*) FROM t WHERE id = 6;   -- returned 0, should be 1
//!
//! The SELECT was routed to the read-only `&self` executor (and the streaming
//! fast path), both of which read the COMMITTED base tables and do not consult
//! the open transaction's uncommitted working set — that state lives in the
//! `&mut` executor's transaction buffer. The embedded API never hit this: it
//! always uses the `&mut` execute path, so its own mvcc self-visibility tests
//! passed while the wire path was broken.
//!
//! The fix routes an in-transaction SELECT to the write path, keyed on the
//! PER-CONNECTION transaction state (not the engine's global flag — the engine
//! is shared across connections, and a global check would drag an autocommit
//! read on one connection onto the write path merely because another connection
//! had a transaction open, causing a dirty read). Autocommit reads keep the
//! shared read lock.
//!
//! This test speaks the PG wire protocol directly, because the bug does not
//! reproduce through the embedded `Engine` API.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new().arg_path(db).with_pgwire().spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-txvis-{label}-{nanos}"));
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
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::new();
    out.push(b'Q');
    let len = (body.len() + 4) as u32;
    out.extend_from_slice(&len.to_be_bytes());
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

fn datarow_cell(body: &[u8], col: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

/// Run `sql`, return the first DataRow's first cell (or None), asserting no
/// ErrorResponse came back.
fn query_one(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    for m in &msgs {
        if m.ty == b'E' {
            let frame = spg_wire::Frame {
                op: spg_wire::Op::ErrorResponse,
                payload: m.body.clone(),
            };
            let txt = spg_wire::parse_error_response(&frame).unwrap_or("<undecodable>");
            panic!("{sql}: unexpected ErrorResponse: {txt}");
        }
    }
    msgs.iter()
        .find(|m| m.ty == b'D')
        .and_then(|m| datarow_cell(&m.body, 0))
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn select_sees_the_transactions_own_writes() {
    let dir = unique_tmpdir("own");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    query_one(&mut s, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut s, "INSERT INTO t VALUES (1, 10), (2, 20)");

    query_one(&mut s, "BEGIN");
    // Own uncommitted INSERT is visible to a later SELECT in the same tx.
    query_one(&mut s, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(query_one(&mut s, "SELECT count(*) FROM t"), Some("3".to_string()));
    // Own uncommitted UPDATE of a pre-existing row is visible.
    query_one(&mut s, "UPDATE t SET v = 999 WHERE id = 1");
    assert_eq!(query_one(&mut s, "SELECT v FROM t WHERE id = 1"), Some("999".to_string()));
    // Own uncommitted DELETE is visible.
    query_one(&mut s, "DELETE FROM t WHERE id = 2");
    assert_eq!(query_one(&mut s, "SELECT count(*) FROM t WHERE id = 2"), Some("0".to_string()));

    // ROLLBACK restores the committed state.
    query_one(&mut s, "ROLLBACK");
    assert_eq!(query_one(&mut s, "SELECT count(*) FROM t"), Some("2".to_string()));
    assert_eq!(query_one(&mut s, "SELECT v FROM t WHERE id = 1"), Some("10".to_string()));
}

#[test]
fn autocommit_read_does_not_see_another_connections_uncommitted_write() {
    // The fix keys on the PER-CONNECTION transaction state; an autocommit read
    // on connection B must NOT be dragged onto the write path (and see
    // uncommitted data) just because connection A has a transaction open.
    let dir = unique_tmpdir("cross");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    let mut a = open(&addr);
    let mut b = open(&addr);

    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN");
    query_one(&mut a, "UPDATE t SET v = 222 WHERE id = 1");
    // B is autocommit; it must see the committed 10, not A's uncommitted 222.
    assert_eq!(query_one(&mut b, "SELECT v FROM t WHERE id = 1"), Some("10".to_string()));
    // …and it must not see A's uncommitted INSERT either.
    query_one(&mut a, "INSERT INTO t VALUES (9, 90)");
    assert_eq!(
        query_one(&mut b, "SELECT count(*) FROM t WHERE id = 9"),
        Some("0".to_string())
    );
    query_one(&mut a, "COMMIT");
    // After A commits, B sees both.
    assert_eq!(query_one(&mut b, "SELECT v FROM t WHERE id = 1"), Some("222".to_string()));
    assert_eq!(
        query_one(&mut b, "SELECT count(*) FROM t WHERE id = 9"),
        Some("1".to_string())
    );
}
