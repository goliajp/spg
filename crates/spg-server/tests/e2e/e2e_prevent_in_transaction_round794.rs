//! r794 — PG's PreventInTransactionBlock family.
//!
//! Statements whose effect no rollback can undo are refused inside an
//! explicit transaction block with SQLSTATE 25001. PG 18.4 was probed
//! for the exact membership before any of this was written, because the
//! line is not where it looks:
//!
//!   refused   VACUUM (bare, with a table, and VACUUM ANALYZE),
//!             ALTER SYSTEM, CREATE DATABASE,
//!             CREATE INDEX CONCURRENTLY, REINDEX … CONCURRENTLY,
//!             DISCARD ALL
//!   allowed   CLUSTER, ANALYZE, plain CREATE INDEX, plain REINDEX
//!
//! The allowed half is half the point. Guarding "CREATE INDEX" or "the
//! no-op DDL path" wholesale would have refused statements PG runs
//! quite happily — plain index builds, and the CREATE ROLE / CREATE
//! CAST family that shares SPG's no-op parse route with CREATE
//! DATABASE. Both halves are pinned here for that reason.

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

fn message(msgs: &[PgMessage]) -> String {
    let Some(m) = msgs.iter().find(|m| m.ty == b'E') else {
        return String::new();
    };
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let Some(rel) = m.body[start..].iter().position(|&b| b == 0) else {
            break;
        };
        let end = start + rel;
        if code == b'M' {
            return String::from_utf8_lossy(&m.body[start..end]).into_owned();
        }
        i = end + 1;
    }
    String::new()
}

fn spawn() -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-preventtx-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

#[test]
fn the_family_pg_refuses_is_refused_with_25001() {
    let (_child, addrs) = spawn();
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT)");
    q(&mut s, "CREATE INDEX i ON t(id)");

    for (sql, named) in [
        ("VACUUM t", "VACUUM"),
        ("VACUUM", "VACUUM"),
        ("VACUUM ANALYZE t", "VACUUM"),
        ("ALTER SYSTEM SET work_mem = '5MB'", "ALTER SYSTEM"),
        ("CREATE DATABASE d794", "CREATE DATABASE"),
        (
            "CREATE INDEX CONCURRENTLY j ON t(id)",
            "CREATE INDEX CONCURRENTLY",
        ),
        ("REINDEX INDEX CONCURRENTLY i", "REINDEX CONCURRENTLY"),
        ("DISCARD ALL", "DISCARD ALL"),
    ] {
        q(&mut s, "BEGIN");
        let msgs = q(&mut s, sql);
        assert_eq!(
            sqlstate(&msgs).as_deref(),
            Some("25001"),
            "`{sql}` inside a transaction block must be PG's 25001"
        );
        let m = message(&msgs);
        assert!(
            m == format!("{named} cannot run inside a transaction block"),
            "`{sql}` should name itself as PG does, got: {m}"
        );
        q(&mut s, "ROLLBACK");
    }
}

#[test]
fn the_ones_pg_allows_there_still_run() {
    let (_child, addrs) = spawn();
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT)");
    q(&mut s, "INSERT INTO t VALUES (1)");
    q(&mut s, "CREATE INDEX i ON t(id)");

    for sql in [
        "ANALYZE t",
        "CLUSTER t USING i",
        "CREATE INDEX i2 ON t(id)",
        "REINDEX INDEX i",
        // Shares SPG's no-op parse route with CREATE DATABASE; PG runs
        // it inside a transaction without complaint. (CREATE ROLE is on
        // that route too and PG allows it there — measured, and it even
        // rolls back — but SPG refuses role DDL in a transaction by its
        // own design, which is a separate divergence in the ledger.)
        "CREATE COLLATION c794 (locale = 'C')",
    ] {
        q(&mut s, "BEGIN");
        let msgs = q(&mut s, sql);
        assert_eq!(
            sqlstate(&msgs),
            None,
            "`{sql}` is allowed inside a transaction block, got: {}",
            message(&msgs)
        );
        q(&mut s, "ROLLBACK");
    }
}

/// Outside a transaction the same statements are unaffected — the guard
/// is about the transaction block, not about the statement.
#[test]
fn autocommit_is_untouched() {
    let (_child, addrs) = spawn();
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    q(&mut s, "CREATE TABLE t (id INT)");
    q(&mut s, "CREATE INDEX i ON t(id)");

    for sql in [
        "VACUUM t",
        "VACUUM",
        "ALTER SYSTEM SET work_mem = '5MB'",
        "CREATE DATABASE d794",
        "CREATE INDEX CONCURRENTLY j ON t(id)",
        "REINDEX INDEX CONCURRENTLY i",
        "DISCARD ALL",
    ] {
        let msgs = q(&mut s, sql);
        assert_eq!(
            sqlstate(&msgs),
            None,
            "`{sql}` in autocommit must still work, got: {}",
            message(&msgs)
        );
    }
}

/// Whether a statement is "inside a transaction" is a property of the
/// connection issuing it, not of the server.
///
/// `exec_create_user` asked the engine-wide `in_transaction()`, which is
/// true while ANY connection holds a transaction, so an autocommit
/// CREATE USER on one connection was refused because an unrelated
/// connection had a BEGIN outstanding — measured before the fix, and a
/// pooled client provisioning users would have seen it come and go.
/// Fourth time this trap has been sprung in this codebase.
#[test]
fn another_connections_transaction_does_not_block_autocommit_ddl() {
    let (_child, addrs) = spawn();
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut holder = open(addr);
    let mut worker = open(addr);

    q(&mut holder, "BEGIN");

    let created = q(&mut worker, "CREATE USER u794 PASSWORD 'x'");
    assert_eq!(
        sqlstate(&created),
        None,
        "the worker is in autocommit; the holder's transaction is not its business, got: {}",
        message(&created)
    );
    let dropped = q(&mut worker, "DROP USER u794");
    assert_eq!(
        sqlstate(&dropped),
        None,
        "same for DROP USER, got: {}",
        message(&dropped)
    );

    // And the connection that really is in a transaction still gets the
    // refusal, so the guard was narrowed rather than removed.
    let refused = q(&mut holder, "CREATE USER u794b PASSWORD 'x'");
    assert!(
        sqlstate(&refused).is_some(),
        "the holder IS in a transaction and role DDL is refused there"
    );
    q(&mut holder, "ROLLBACK");
}

