//! v7.39 (round 283) — one transaction slot PER CONNECTION.
//!
//! The server runs ONE shared `Engine`, and every pgwire path called
//! `Engine::execute()`, which routes through `IMPLICIT_TX` — slot 0. So
//! all connections shared a single transaction. Two consequences, both
//! measured against live PG 18.4 before the fix:
//!
//!   * a second client's `BEGIN` answered `a transaction is already
//!     open`, and that client's session then sat in the aborted state;
//!   * a READ COMMITTED transaction never saw another connection's
//!     commit — not because the isolation was wrong, but because there
//!     was no other connection's transaction to see. The engine's own
//!     pins had this right all along: they drive `execute_in(sql, tx)`
//!     with `alloc_tx_id()`, which the server never called.
//!
//! The engine has been multi-slot since v4.41.1. This round is the
//! server finally asking for a slot.
//!
//! These must speak the PG wire directly — the defect is in how the
//! server addresses the engine, so the embedded API cannot reach it.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-conctx-{label}-{nanos}"));
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
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
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
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
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

/// Run `sql`; panic on ErrorResponse. Returns the first cell of the
/// first DataRow.
fn query_one(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    for m in &msgs {
        assert!(m.ty != b'E', "{sql}: unexpected ErrorResponse");
    }
    msgs.iter()
        .find(|m| m.ty == b'D')
        .and_then(|m| datarow_cell(&m.body, 0))
}

/// Run `sql` expecting it to SUCCEED or FAIL; returns the error text.
fn query_err(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    msgs.iter().find(|m| m.ty == b'E').map(|m| {
        let frame = spg_wire::Frame {
            op: spg_wire::Op::ErrorResponse,
            payload: m.body.clone(),
        };
        spg_wire::parse_error_response(&frame)
            .unwrap_or("<undecodable>")
            .to_string()
    })
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn boot(label: &str) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    (common::ChildGuard(raw), addr)
}

#[test]
fn two_connections_can_hold_transactions_at_the_same_time() {
    let (_child, addr) = boot("two");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    // Before this round: "a transaction is already open", and B's
    // session was poisoned for every statement that followed.
    assert_eq!(query_err(&mut b, "BEGIN"), None);
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut b, "INSERT INTO t VALUES (2, 20)");
    query_one(&mut a, "COMMIT");
    query_one(&mut b, "COMMIT");

    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM t"), Some("2".into()));
}

#[test]
fn one_connections_uncommitted_write_is_invisible_to_another() {
    let (_child, addr) = boot("dirty");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    // The slots are separate, so B must not read A's uncommitted row.
    assert_eq!(query_one(&mut b, "SELECT count(*) FROM t"), Some("0".into()));
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut b, "SELECT count(*) FROM t"), Some("1".into()));
}

#[test]
fn a_rollback_on_one_connection_leaves_the_other_alone() {
    let (_child, addr) = boot("rb");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    query_one(&mut b, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut b, "INSERT INTO t VALUES (2, 20)");
    query_one(&mut a, "ROLLBACK");
    query_one(&mut b, "COMMIT");

    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM t"), Some("1".into()));
    assert_eq!(
        query_one(&mut c, "SELECT v FROM t WHERE id = 2"),
        Some("20".into()),
    );
}

#[test]
fn read_committed_sees_another_connections_commit() {
    // PG 18.4, same script: the second read returns 99. Before this
    // round SPG returned 10 — which looked like READ COMMITTED being
    // silently upgraded to REPEATABLE READ, and was really both
    // sessions sharing one transaction slot.
    let (_child, addr) = boot("rc");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL READ COMMITTED");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
    );
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("99".into()),
        "READ COMMITTED takes a fresh snapshot per statement",
    );
    query_one(&mut a, "COMMIT");
}

#[test]
fn repeatable_read_still_freezes_its_view() {
    // The other half of the same story: RR must NOT see the commit.
    // This was already true, but only provably so once two connections
    // could hold transactions at once.
    let (_child, addr) = boot("rr");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
    );
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
        "REPEATABLE READ holds the snapshot it took at BEGIN",
    );
    query_one(&mut a, "COMMIT");
}

#[test]
fn a_disconnect_mid_transaction_does_not_strand_the_slot() {
    // A client that vanishes inside BEGIN must not leave its shadow
    // catalog — with uncommitted rows — sitting in the engine.
    let (_child, addr) = boot("drop");
    let mut a = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    {
        let mut doomed = open(&addr);
        query_one(&mut doomed, "BEGIN");
        query_one(&mut doomed, "INSERT INTO t VALUES (7, 70)");
        // dropped without COMMIT or ROLLBACK
    }
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("0".into()));
    // …and the engine still accepts new transactions afterwards.
    query_one(&mut a, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (8, 80)");
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("1".into()));
}
