//! 9.0.3 — an autocommit `SELECT … FOR UPDATE` never let the row go.
//!
//! Measured on the published 9.0.3 image against PostgreSQL 18.6, with
//! `statement_timeout = '3s'` on the statement that has to wait:
//!
//! ```text
//!   same connection, then UPDATE            PG: UPDATE 1, v = 11
//!                                           SPG: canceling statement due to
//!                                                statement timeout, v = 10
//!   another connection, then UPDATE         PG: UPDATE 1
//!                                           SPG: canceling statement …
//!   another connection, the first one       PG: canceling statement …
//!   holding the row inside a transaction    SPG: canceling statement …
//! ```
//!
//! Two causes, one behind the other. The locking prepass took its locks
//! under `unwrap_or(0)` — version 0 is nobody — so the release at the end
//! of an autocommit statement, which frees that statement's own writer
//! version, never matched them. And because every autocommit statement
//! shared that one version, two connections locking the same row did not
//! conflict with each other either; the row stayed locked for whoever came
//! next, from any connection, until the server stopped.
//!
//! The third shape is the guard rail: inside a transaction the lock is
//! supposed to outlive the statement, and it still does.

use crate::common;
use crate::e2e_copy_903::{Copied, ok, open, rows, run};
use std::net::TcpStream;

fn server(name: &str) -> (common::ChildGuard, String) {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs.pgwire.unwrap())
}

/// A connection with a short `statement_timeout`, so a statement that
/// waits for a lock fails instead of hanging this test.
fn conn(addr: &str) -> TcpStream {
    let mut c = open(addr);
    ok(&mut c, "SET statement_timeout = '3s'");
    c
}

fn timed_out(r: &Copied) -> bool {
    r.error
        .as_deref()
        .is_some_and(|e| e.contains("canceling statement due to statement timeout"))
}

#[test]
fn an_autocommit_for_update_releases_the_row_with_the_statement() {
    let (_child, addr) = server("aclock903");
    let mut a = conn(&addr);
    ok(&mut a, "CREATE TABLE p10 (id int PRIMARY KEY, v int)");
    ok(&mut a, "INSERT INTO p10 VALUES (1, 10)");

    // The same connection. PostgreSQL: the row, then UPDATE 1, then 11.
    assert_eq!(
        rows(&mut a, "SELECT id FROM p10 WHERE id = 1 FOR UPDATE"),
        "1\n"
    );
    let r = run(&mut a, "UPDATE p10 SET v = v + 1 WHERE id = 1", "");
    assert_eq!(r.error, None, "the row is this statement's to take");
    assert_eq!(r.tag.as_deref(), Some("UPDATE 1"));
    assert_eq!(rows(&mut a, "SELECT v FROM p10 WHERE id = 1"), "11\n");

    // Another connection, the first one still open. Its lock ended with
    // its statement, so this UPDATE goes through.
    let mut b = conn(&addr);
    let r = run(&mut b, "UPDATE p10 SET v = v + 100 WHERE id = 1", "");
    assert_eq!(r.error, None, "connection A's autocommit lock is over");
    assert_eq!(r.tag.as_deref(), Some("UPDATE 1"));
    assert_eq!(rows(&mut a, "SELECT v FROM p10 WHERE id = 1"), "111\n");
}

#[test]
fn inside_a_transaction_the_lock_outlives_the_statement() {
    let (_child, addr) = server("txlock903");
    let mut a = conn(&addr);
    ok(&mut a, "CREATE TABLE p11 (id int PRIMARY KEY, v int)");
    ok(&mut a, "INSERT INTO p11 VALUES (1, 10)");

    ok(&mut a, "BEGIN");
    assert_eq!(
        rows(&mut a, "SELECT id FROM p11 WHERE id = 1 FOR UPDATE"),
        "1\n"
    );

    let mut b = conn(&addr);
    let r = run(&mut b, "UPDATE p11 SET v = v + 1 WHERE id = 1", "");
    assert!(
        timed_out(&r),
        "A holds the row until it commits: {:?}",
        r.error
    );

    ok(&mut a, "COMMIT");
    let r = run(&mut b, "UPDATE p11 SET v = v + 1 WHERE id = 1", "");
    assert_eq!(r.error, None, "and lets it go when it does");
    assert_eq!(r.tag.as_deref(), Some("UPDATE 1"));
}
