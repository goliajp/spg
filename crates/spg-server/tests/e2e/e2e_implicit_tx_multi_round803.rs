//! r803 — a multi-statement simple query is one implicit transaction.
//!
//! PG wraps everything in a single Q message in one transaction, so a
//! script that fails halfway leaves nothing behind. SPG dispatched per
//! statement and left the earlier ones applied: measured on PG 18.4 and
//! against SPG with the wrap removed, `INSERT INTO t VALUES (1); INSERT
//! INTO t VALUES (1);` against a primary key leaves 0 rows there and
//! left 1 here.
//!
//! Every rule below was probed on PG 18.4 first. Three of them fall out
//! of implementing the wrap as a real transaction rather than as
//! bookkeeping, which is why they are pinned together — a reimplementation
//! that special-cases the rollback would have to rediscover each one:
//!
//!   * VACUUM alone in a message runs; VACUUM with a sibling statement
//!     reports 25001, because the wrap really is a transaction block and
//!     round 794's guard sees it;
//!   * an explicit COMMIT mid-script closes the wrap, so statements after
//!     it stand on their own even if a later one fails;
//!   * an explicit BEGIN…COMMIT inside the script behaves as before,
//!     since the wrap only opens on an idle connection.
//!
//! One caution for anyone extending these. The comparison has to send
//! the same thing to both servers: `psql -c 'a; b;'` puts the whole
//! string in one Q frame, while a heredoc lets psql parse it and send a
//! frame per statement. The first version of this investigation used
//! `-c` against PG and a heredoc against SPG, which is not a like-for-
//! like test — the divergence it reported was real, but the evidence was
//! not. Use `-c`.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
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

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

/// One Q frame carrying the whole string — the shape `psql -c` sends,
/// and the only shape that exercises the multi-statement path.
fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn sqlstate(msgs: &[PgMessage]) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == b'C' {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn one_value(s: &mut TcpStream, sql: &str) -> String {
    let msgs = q(s, sql);
    let d = msgs.iter().find(|m| m.ty == b'D').expect("a DataRow");
    let len = i32::from_be_bytes([d.body[2], d.body[3], d.body[4], d.body[5]]);
    String::from_utf8_lossy(&d.body[6..6 + len as usize]).into_owned()
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-implicittx-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

#[test]
fn a_script_that_fails_halfway_leaves_nothing_behind() {
    let (_child, addrs) = spawn("rollback");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");

    let msgs = q(
        &mut s,
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (1);",
    );
    assert!(
        sqlstate(&msgs).is_some(),
        "the second insert violates the key"
    );

    assert_eq!(
        one_value(&mut s, "SELECT count(*) FROM t"),
        "0",
        "the first insert rolls back with the second, as it does on PG"
    );
}

#[test]
fn a_script_that_succeeds_commits_all_of_it() {
    let (_child, addrs) = spawn("commit");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");

    let msgs = q(
        &mut s,
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (3);",
    );
    assert_eq!(sqlstate(&msgs), None);
    assert_eq!(one_value(&mut s, "SELECT count(*) FROM t"), "3");
}

/// PG's rule, measured: allowed alone in a message, refused with a
/// sibling. The wrap being a real transaction is what makes the second
/// half true — round 794's guard fires on its own.
#[test]
fn vacuum_runs_alone_and_is_refused_with_a_sibling() {
    let (_child, addrs) = spawn("vacuum");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");

    let alone = q(&mut s, "VACUUM t");
    assert_eq!(
        sqlstate(&alone),
        None,
        "a message whose only statement is VACUUM is not wrapped"
    );

    let with_sibling = q(&mut s, "SELECT 1; VACUUM t;");
    assert_eq!(
        sqlstate(&with_sibling).as_deref(),
        Some("25001"),
        "with a sibling it is inside the implicit transaction, and PG \
         reports 25001 there too"
    );
}

/// PG leaves 2 rows for this, because the COMMIT ends the implicit
/// transaction and the rows after it are their own unit.
#[test]
fn an_explicit_commit_mid_script_closes_the_wrap() {
    let (_child, addrs) = spawn("midcommit");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
    q(&mut s, "INSERT INTO t VALUES (1)");

    let msgs = q(
        &mut s,
        "INSERT INTO t VALUES (2); COMMIT; INSERT INTO t VALUES (3); SELECT 1/0;",
    );
    assert!(sqlstate(&msgs).is_some(), "the division fails the script");

    assert_eq!(
        one_value(&mut s, "SELECT count(*) FROM t"),
        "2",
        "row 2 was committed by the explicit COMMIT; row 3 went with the \
         failure. PG leaves the same two."
    );
}

/// The wrap only opens on an idle connection, so a script that manages
/// its own transaction is untouched.
#[test]
fn an_explicit_transaction_inside_the_script_still_works() {
    let (_child, addrs) = spawn("explicit");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");

    let msgs = q(
        &mut s,
        "BEGIN; INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2);",
    );
    assert_eq!(sqlstate(&msgs), None);
    assert_eq!(one_value(&mut s, "SELECT count(*) FROM t"), "2");
}

/// A single-statement message is not a script and must not be wrapped —
/// otherwise every ordinary query pays a transaction it did not ask for.
#[test]
fn a_single_statement_message_is_not_wrapped() {
    let (_child, addrs) = spawn("single");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");

    let msgs = q(&mut s, "INSERT INTO t VALUES (1)");
    assert_eq!(sqlstate(&msgs), None);
    assert_eq!(one_value(&mut s, "SELECT count(*) FROM t"), "1");
}
