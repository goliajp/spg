//! v7.39 (read01 round 85 follow-up / round 86) — a statement error inside a
//! transaction must abort the whole block (PG's 25P02), over the wire.
//!
//! A differential sweep found that after a statement errored inside a BEGIN
//! block, SPG kept serving subsequent statements: a SELECT still returned rows,
//! a SHOW still returned its value, a SET silently applied, and a COMMIT
//! actually committed. PG rejects every non-transaction-control statement with
//! `current transaction is aborted, commands ignored until end of transaction
//! block` (SQLSTATE 25P02) and downgrades a COMMIT to a ROLLBACK.
//!
//! The engine already had the firewall (`execute_stmt_with_cancel` sets
//! tx_aborted), but two wire paths bypassed it: the read-only `&self` executor
//! (SELECT), and the short-circuit handlers that answer SHOW / SET / canned
//! probes before the execute path is reached. The read path is now routed to
//! the firewalled write path when the connection is in a transaction — open or
//! aborted — and a wire-level guard rejects the short-circuit statements in an
//! aborted block. A COMMIT there is tagged ROLLBACK.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-abort-{label}-{nanos}"));
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

/// SQLSTATE from an ErrorResponse body (field 'C').
fn error_sqlstate(body: &[u8]) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let code = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        let val = std::str::from_utf8(&body[start..end]).unwrap_or("");
        if code == b'C' {
            return Some(val.to_string());
        }
        p = end + 1;
    }
    None
}

/// CommandComplete tag body is a null-terminated string.
fn command_tag(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    std::str::from_utf8(&body[..end]).unwrap_or("").to_string()
}

enum Outcome {
    Rows(usize),
    Tag(String),
    Error(String), // SQLSTATE
}

fn run(s: &mut TcpStream, sql: &str) -> Outcome {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        return Outcome::Error(error_sqlstate(&e.body).unwrap_or_default());
    }
    let rows = msgs.iter().filter(|m| m.ty == b'D').count();
    if msgs.iter().any(|m| m.ty == b'D' || m.ty == b'T') {
        return Outcome::Rows(rows);
    }
    let tag = msgs
        .iter()
        .find(|m| m.ty == b'C')
        .map(|m| command_tag(&m.body))
        .unwrap_or_default();
    Outcome::Tag(tag)
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn assert_25p02(o: Outcome, ctx: &str) {
    match o {
        Outcome::Error(code) => assert_eq!(code, "25P02", "{ctx}: wrong SQLSTATE"),
        Outcome::Rows(n) => panic!("{ctx}: expected 25P02, got {n} rows"),
        Outcome::Tag(t) => panic!("{ctx}: expected 25P02, got tag {t}"),
    }
}

#[test]
fn statement_error_aborts_the_transaction() {
    let dir = unique_tmpdir("abort");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    matches!(
        run(&mut s, "CREATE TABLE z (id int primary key, v int)"),
        Outcome::Tag(_)
    );
    matches!(run(&mut s, "INSERT INTO z VALUES (1, 10)"), Outcome::Tag(_));

    matches!(run(&mut s, "BEGIN"), Outcome::Tag(_));
    // The error that aborts the block.
    assert!(
        matches!(run(&mut s, "INSERT INTO z VALUES (1, 99)"), Outcome::Error(c) if c == "23505"),
        "duplicate insert should be 23505"
    );
    // Every non-tx-control statement now rejects with 25P02, whichever wire path
    // it would have taken.
    assert_25p02(run(&mut s, "SELECT count(*) FROM z"), "SELECT (read path)");
    assert_25p02(
        run(&mut s, "INSERT INTO z VALUES (2, 20)"),
        "INSERT (write path)",
    );
    assert_25p02(
        run(&mut s, "SHOW transaction_isolation"),
        "SHOW (short-circuit)",
    );
    assert_25p02(
        run(&mut s, "SET application_name = 'x'"),
        "SET (short-circuit)",
    );

    // COMMIT in an aborted tx rolls back and is tagged ROLLBACK.
    match run(&mut s, "COMMIT") {
        Outcome::Tag(t) => assert_eq!(t, "ROLLBACK"),
        other => panic!(
            "COMMIT-in-aborted: expected ROLLBACK tag, got {}",
            match other {
                Outcome::Rows(n) => format!("{n} rows"),
                Outcome::Error(c) => format!("error {c}"),
                Outcome::Tag(_) => unreachable!(),
            }
        ),
    }
    // The aborted work did not apply; the transaction is over.
    assert!(matches!(
        run(&mut s, "SELECT count(*) FROM z"),
        Outcome::Rows(1)
    ));
}

#[test]
fn rollback_recovers_and_missing_column_also_aborts() {
    let dir = unique_tmpdir("rb");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    matches!(
        run(&mut s, "CREATE TABLE z (id int primary key)"),
        Outcome::Tag(_)
    );
    matches!(run(&mut s, "INSERT INTO z VALUES (1)"), Outcome::Tag(_));

    matches!(run(&mut s, "BEGIN"), Outcome::Tag(_));
    // A missing-column SELECT is the error that aborts.
    matches!(
        run(&mut s, "SELECT nonexistent_col FROM z"),
        Outcome::Error(_)
    );
    assert_25p02(run(&mut s, "SELECT 1"), "SELECT after missing-col");
    // ROLLBACK ends the aborted block and recovers the session.
    match run(&mut s, "ROLLBACK") {
        Outcome::Tag(t) => assert_eq!(t, "ROLLBACK"),
        _ => panic!("ROLLBACK should tag ROLLBACK"),
    }
    // Session works again.
    assert!(matches!(
        run(&mut s, "SELECT count(*) FROM z"),
        Outcome::Rows(1)
    ));
}
