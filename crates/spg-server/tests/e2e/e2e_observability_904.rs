//! 9.0.4 — what an operator can see while the database is working.
//!
//! Measured on the published 9.0.3 against PostgreSQL 18.6, both under
//! the customer's ingest workload, by `xtests/gates/g5-observe.sh`:
//!
//! ```text
//!   which transaction is it in   SPG: no rows    PG: the running one
//!   locks held                   SPG: 0 rows     PG: 11 rows
//! ```
//!
//! `pg_stat_activity.xact_start` was NULL for every connection and no
//! connection could ever read `idle in transaction` — the state an
//! operator goes looking for. The flag behind it is one the host keeps,
//! and on the PostgreSQL wire nothing ever wrote it; only the MySQL wire
//! did. The engine knows, so the view now asks the engine.
//!
//! `pg_locks` returned an empty row set in every build from v7.37.14 on.
//! Its comment said "empty until v7.37.15"; v7.37.15 built the lock
//! table and nobody came back. And the first row it produced once it did
//! was refused by its own schema, which declared `waitstart_us` NOT NULL
//! where PostgreSQL's `waitstart` is nullable.

use crate::common;
use crate::e2e_copy_903::{ok, open, rows};
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

fn one(c: &mut TcpStream, sql: &str) -> String {
    rows(c, sql).trim_end().to_string()
}

#[test]
fn an_open_transaction_and_its_locks_are_visible_from_another_connection() {
    let (_child, addr) = server("obs904");
    let mut a = open(&addr);
    ok(&mut a, "CREATE TABLE t (id int PRIMARY KEY, v int)");
    ok(&mut a, "INSERT INTO t VALUES (1, 10)");

    let mut b = open(&addr);
    // A ran two statements and is running none. PostgreSQL 18.6 reports
    // `idle`; SPG wrote the statement into the view and never took it
    // out, so every open connection read `active` for ever — and
    // `idle in transaction`, below, was unreachable for that reason too.
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_stat_activity              WHERE backend_type = 'client backend' AND state = 'idle'"
        ),
        "1",
        "A is idle",
    );
    // Nothing is locked and nobody is in a transaction yet — the floor
    // this test needs, or the assertions below would pass on a view
    // that answers the same thing whatever the server is doing.
    assert_eq!(one(&mut b, "SELECT count(*) FROM pg_locks"), "0");
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'idle in transaction'"
        ),
        "0",
    );

    ok(&mut a, "BEGIN");
    assert_eq!(
        rows(&mut a, "SELECT id FROM t WHERE id = 1 FOR UPDATE"),
        "1\n"
    );

    // A is now sitting in an open transaction holding a row.
    let seen = one(
        &mut b,
        "SELECT pid, state, xact_start IS NOT NULL, query FROM pg_stat_activity          WHERE backend_type = 'client backend'",
    );
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'idle in transaction'"
        ),
        "1",
        "PostgreSQL names this state; SPG could not reach it. Rows:\n{seen}",
    );
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_stat_activity \
             WHERE state = 'idle in transaction' AND xact_start IS NOT NULL"
        ),
        "1",
        "and says when the transaction began",
    );
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_locks WHERE relation = 't' AND granted"
        ),
        "1",
        "the row A holds",
    );
    assert_eq!(
        one(&mut b, "SELECT locktype, mode FROM pg_locks"),
        "tuple|AccessExclusiveLock",
    );

    // And both go back when the transaction ends.
    ok(&mut a, "COMMIT");
    assert_eq!(one(&mut b, "SELECT count(*) FROM pg_locks"), "0");
    assert_eq!(
        one(
            &mut b,
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'idle in transaction'"
        ),
        "0",
    );
}
