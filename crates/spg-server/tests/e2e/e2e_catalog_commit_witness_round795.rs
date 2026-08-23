//! r795 — an autocommit DDL is persisted and audited regardless of what
//! other connections are doing.
//!
//! `QueryResult::modified_catalog` tells the server "this took effect
//! now, persist and audit it". The engine computed it from the
//! engine-wide `in_transaction()`, which is true while ANY connection
//! holds a transaction, so a connection in autocommit reported its DDL
//! as uncommitted whenever an unrelated connection sat inside a BEGIN.
//!
//! Both consequences were measured before the fix, with a second
//! connection idling in a transaction as the only difference:
//!
//!   * the statement was missing from the audit log entirely — the
//!     control DDL had its line, the one issued alongside the open
//!     transaction had none;
//!   * in no-WAL mode the table it created was acked to the client,
//!     visible in `pg_tables`, and gone after `kill -9` and a restart,
//!     while the control table survived.
//!
//! An audit log that drops entries depending on unrelated connection
//! state is worse than no audit log, and a CREATE TABLE that reports
//! success and vanishes is the plainest durability bug there is.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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

fn errored(msgs: &[PgMessage]) -> bool {
    msgs.iter().any(|m| m.ty == b'E')
}

/// Every DataRow's first column, as text.
fn col0(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            continue;
        }
        out.push(String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned());
    }
    out
}

fn unique_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-witness-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_on(db: &Path, audit: Option<&Path>) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new().arg_path(db).with_pgwire();
    if let Some(a) = audit {
        b = b.env("SPG_AUDIT", a.to_string_lossy().into_owned());
    }
    b.spawn()
}

#[test]
fn ddl_is_audited_even_while_another_connection_holds_a_transaction() {
    let dir = unique_dir("audit");
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");
    let (raw, addrs) = spawn_on(&db, Some(&audit));
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();

    let mut worker = open(addr);
    assert!(!errored(&q(
        &mut worker,
        "CREATE TABLE control_tbl (id INT)"
    )));

    let mut holder = open(addr);
    q(&mut holder, "BEGIN");
    assert!(!errored(&q(
        &mut worker,
        "CREATE TABLE victim_tbl (id INT)"
    )));
    q(&mut holder, "COMMIT");

    // Read the chain through its own view rather than grepping the file:
    // that is the surface the audit feature supports, and the on-disk
    // shape differs between WAL and no-WAL servers.
    let audited: Vec<String> = q(&mut worker, "SELECT sql FROM spg_audit_chain")
        .iter()
        .filter(|m| m.ty == b'D')
        .map(|m| {
            let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
            if len < 0 {
                return String::new();
            }
            String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned()
        })
        .collect();
    let _ = &audit;

    assert!(
        audited.iter().any(|e| e.contains("control_tbl")),
        "the control DDL should be in the audit chain, got {audited:?}"
    );
    assert!(
        audited.iter().any(|e| e.contains("victim_tbl")),
        "so should the one issued while another connection held a \
         transaction — the audit chain does not get to depend on that, \
         got {audited:?}"
    );
}

#[test]
fn ddl_survives_a_crash_even_while_another_connection_holds_a_transaction() {
    let dir = unique_dir("durable");
    let db = dir.join("spg.db");
    let (raw, addrs) = spawn_on(&db, None);
    let addr = addrs.pgwire.as_ref().unwrap().clone();

    let mut worker = open(&addr);
    assert!(!errored(&q(
        &mut worker,
        "CREATE TABLE control_tbl (id INT)"
    )));

    // The holder's transaction is never committed, and nothing writes
    // after the victim DDL — otherwise a later write's snapshot would
    // carry it to disk and hide the gap.
    let mut holder = open(&addr);
    q(&mut holder, "BEGIN");
    assert!(!errored(&q(
        &mut worker,
        "CREATE TABLE victim_tbl (id INT)"
    )));

    let before = col0(&q(
        &mut worker,
        "SELECT tablename FROM pg_tables WHERE tablename LIKE '%_tbl' ORDER BY 1",
    ));
    assert_eq!(
        before,
        vec!["control_tbl", "victim_tbl"],
        "both are live before the crash"
    );

    let mut raw = raw;
    raw.kill().expect("kill the server");
    raw.wait().ok();
    drop(worker);
    drop(holder);

    let (raw2, addrs2) = spawn_on(&db, None);
    let _child2 = common::ChildGuard(raw2);
    let mut after_conn = open(addrs2.pgwire.as_ref().unwrap());
    let after = col0(&q(
        &mut after_conn,
        "SELECT tablename FROM pg_tables WHERE tablename LIKE '%_tbl' ORDER BY 1",
    ));
    assert_eq!(
        after,
        vec!["control_tbl", "victim_tbl"],
        "a CREATE TABLE that was acked must still be there; before the fix \
         victim_tbl was gone and control_tbl was not"
    );
}

/// r797 — the same gap, still open in the DROP family.
///
/// Round 795 replaced `modified_catalog: !self.in_transaction()`
/// wherever it appeared verbatim. Six sites wrote it as
/// `removed > 0 && !self.in_transaction()` — DROP DOMAIN, DROP SCHEMA,
/// DROP TYPE, DROP MATERIALIZED VIEW, DROP VIEW, DROP SEQUENCE — and a
/// textual replacement walked straight past them. Measured before the
/// fix: DROP VIEW with another connection holding a transaction reported
/// success, and the view was back after kill -9 and a restart.
///
/// A dropped object coming back is the mirror of round 795's created
/// table going away, and it is the reason this round audited all
/// forty-five `in_transaction()` sites by hand instead of matching the
/// pattern once more.
#[test]
fn a_drop_stays_dropped_even_while_another_connection_holds_a_transaction() {
    let dir = unique_dir("dropfamily");
    let db = dir.join("spg.db");
    let (raw, addrs) = spawn_on(&db, None);
    let addr = addrs.pgwire.as_ref().unwrap().clone();

    let mut worker = open(&addr);
    assert!(!errored(&q(&mut worker, "CREATE TABLE t (id INT)")));
    assert!(!errored(&q(
        &mut worker,
        "CREATE VIEW control_v AS SELECT id FROM t"
    )));
    assert!(!errored(&q(
        &mut worker,
        "CREATE VIEW victim_v AS SELECT id FROM t"
    )));
    // Gets the pre-drop state to disk with nobody else in a transaction.
    assert!(!errored(&q(&mut worker, "DROP VIEW control_v")));

    let mut holder = open(&addr);
    q(&mut holder, "BEGIN");
    assert!(!errored(&q(&mut worker, "DROP VIEW victim_v")));

    let mut raw = raw;
    raw.kill().expect("kill the server");
    raw.wait().ok();
    drop(worker);
    drop(holder);

    let (raw2, addrs2) = spawn_on(&db, None);
    let _child2 = common::ChildGuard(raw2);
    let mut after_conn = open(addrs2.pgwire.as_ref().unwrap());
    let views = col0(&q(
        &mut after_conn,
        "SELECT viewname FROM pg_views WHERE viewname LIKE '%_v' ORDER BY 1",
    ));
    assert!(
        views.is_empty(),
        "both DROPs were acked; before the fix victim_v came back, got {views:?}"
    );
}
