//! 7.38.1 S2.4 (MATRIX #20) — pgbench tpcb's 1.7% "duplicate key"
//! 40001s reproduced at engine level: a tx whose UPDATE was already
//! rebased ONCE gets rebased again (every concurrent commit moves the
//! epoch), and the second rebase's unique pre-check must not mistake
//! the tx's own re-staged UPDATE for a concurrently-taken key.

use spg_engine::{Engine, QueryResult};

fn one_cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// T1 updates a hot row, then two OTHER transactions commit (each
/// moving the commit epoch, forcing a rebase before T1's next
/// statement and again at COMMIT). T1's own update must survive both
/// rebases and its COMMIT must not report a duplicate key.
#[test]
fn pin_v7381_double_rebase_keeps_own_update() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tellers (tid INT PRIMARY KEY, tbalance INT)")
        .unwrap();
    e.execute("CREATE TABLE branches (bid INT PRIMARY KEY, bbalance INT)")
        .unwrap();
    e.execute("INSERT INTO tellers VALUES (1, 0), (2, 0), (3, 0)")
        .unwrap();
    e.execute("INSERT INTO branches VALUES (1, 0)").unwrap();

    let t1 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 10 WHERE tid = 1",
        t1,
    )
    .unwrap();

    // Concurrent commit #1 (disjoint row) — moves the epoch.
    let t2 = e.alloc_tx_id();
    e.execute_in("BEGIN", t2).unwrap();
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 5 WHERE tid = 2",
        t2,
    )
    .unwrap();
    e.execute_in("COMMIT", t2).unwrap();

    // T1's next statement triggers rebase #1.
    e.execute_in(
        "UPDATE branches SET bbalance = bbalance + 10 WHERE bid = 1",
        t1,
    )
    .unwrap();

    // Concurrent commit #2 — moves the epoch again.
    let t3 = e.alloc_tx_id();
    e.execute_in("BEGIN", t3).unwrap();
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 7 WHERE tid = 3",
        t3,
    )
    .unwrap();
    e.execute_in("COMMIT", t3).unwrap();

    // COMMIT triggers rebase #2 — the pgbench failure fired HERE
    // (or on a later statement), claiming T1's own teller key was a
    // concurrently-taken duplicate.
    e.execute_in("COMMIT", t1)
        .expect("own update must survive a second rebase");

    assert_eq!(
        one_cell(&mut e, "SELECT tbalance FROM tellers WHERE tid = 1"),
        "10"
    );
    assert_eq!(
        one_cell(&mut e, "SELECT tbalance FROM tellers WHERE tid = 2"),
        "5"
    );
    assert_eq!(
        one_cell(&mut e, "SELECT tbalance FROM tellers WHERE tid = 3"),
        "7"
    );
    assert_eq!(one_cell(&mut e, "SELECT count(*) FROM tellers"), "3");
}

/// Same shape but the in-between statement UPDATES A SECOND ROW of
/// the same hot table before the next rebase — the tx then carries
/// two update pairs through consecutive rebases.
#[test]
fn pin_v7381_double_rebase_two_pairs_same_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tellers (tid INT PRIMARY KEY, tbalance INT)")
        .unwrap();
    e.execute("INSERT INTO tellers VALUES (1, 0), (2, 0), (3, 0), (4, 0)")
        .unwrap();

    let t1 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("UPDATE tellers SET tbalance = 11 WHERE tid = 1", t1)
        .unwrap();

    let t2 = e.alloc_tx_id();
    e.execute_in("BEGIN", t2).unwrap();
    e.execute_in("UPDATE tellers SET tbalance = 33 WHERE tid = 3", t2)
        .unwrap();
    e.execute_in("COMMIT", t2).unwrap();

    // Rebase #1 happens before this statement; it stages a second pair.
    e.execute_in("UPDATE tellers SET tbalance = 22 WHERE tid = 2", t1)
        .unwrap();

    let t3 = e.alloc_tx_id();
    e.execute_in("BEGIN", t3).unwrap();
    e.execute_in("UPDATE tellers SET tbalance = 44 WHERE tid = 4", t3)
        .unwrap();
    e.execute_in("COMMIT", t3).unwrap();

    e.execute_in("COMMIT", t1)
        .expect("both pairs must survive rebase #2");

    assert_eq!(one_cell(&mut e, "SELECT count(*) FROM tellers"), "4");
    assert_eq!(
        one_cell(
            &mut e,
            "SELECT string_agg(tbalance::text, ',' ORDER BY tid) FROM tellers"
        ),
        "11,22,33,44"
    );
}

/// The full pgbench shape: T_b's teller UPDATE would-blocks behind
/// T_a, retries after T_a's COMMIT (tombstoning T_a's fresh version),
/// then hits another rebase before its own COMMIT. This is the exact
/// interleaving behind the 1.7% "duplicate key" 40001s.
#[test]
fn pin_v7381_lock_retry_then_rebase_keeps_own_update() {
    use spg_engine::EngineError;
    let mut e = Engine::new();
    e.execute("CREATE TABLE tellers (tid INT PRIMARY KEY, tbalance INT)")
        .unwrap();
    e.execute("CREATE TABLE branches (bid INT PRIMARY KEY, bbalance INT)")
        .unwrap();
    e.execute("INSERT INTO tellers VALUES (8, 0), (9, 0)")
        .unwrap();
    e.execute("INSERT INTO branches VALUES (1, 0)").unwrap();

    let ta = e.alloc_tx_id();
    let tb = e.alloc_tx_id();
    e.execute_in("BEGIN", ta).unwrap();
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 1 WHERE tid = 8",
        ta,
    )
    .unwrap();
    e.execute_in("BEGIN", tb).unwrap();
    let err = e
        .execute_in(
            "UPDATE tellers SET tbalance = tbalance + 2 WHERE tid = 8",
            tb,
        )
        .expect_err("must block behind ta");
    assert!(matches!(err, EngineError::LockWouldBlock), "{err:?}");
    e.execute_in("COMMIT", ta).unwrap();
    // Server-style retry after the holder's COMMIT.
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 2 WHERE tid = 8",
        tb,
    )
    .unwrap();
    // Epoch moves again under tb's feet.
    let tc = e.alloc_tx_id();
    e.execute_in("BEGIN", tc).unwrap();
    e.execute_in(
        "UPDATE tellers SET tbalance = tbalance + 5 WHERE tid = 9",
        tc,
    )
    .unwrap();
    e.execute_in("COMMIT", tc).unwrap();
    // pgbench failed HERE (the statement after the retried one).
    e.execute_in(
        "UPDATE branches SET bbalance = bbalance + 2 WHERE bid = 1",
        tb,
    )
    .unwrap();
    e.execute_in("COMMIT", tb).unwrap();
    assert_eq!(
        one_cell(&mut e, "SELECT tbalance FROM tellers WHERE tid = 8"),
        "3"
    );
    assert_eq!(one_cell(&mut e, "SELECT count(*) FROM tellers"), "2");
}
