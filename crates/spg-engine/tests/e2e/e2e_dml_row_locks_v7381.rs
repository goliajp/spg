//! 7.38.1 S2.1 (MATRIX #20) — UPDATE/DELETE take tuple locks, so a
//! concurrent same-row writer gets `LockWouldBlock` at STATEMENT time
//! (the server retries outside the engine guard) instead of racing to
//! a 40001 — or a silently lost update — at COMMIT.

use spg_engine::{Engine, EngineError, IMPLICIT_TX, QueryResult};

fn one_cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// tx holder vs tx challenger: the challenger's UPDATE would-blocks,
/// and after the holder commits it retries through and applies.
#[test]
fn pin_v7381_update_conflict_would_blocks_then_applies() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ub (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO ub VALUES (1, 10)").unwrap();
    let t1 = e.alloc_tx_id();
    let t2 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("UPDATE ub SET v = 20 WHERE id = 1", t1)
        .unwrap();
    e.execute_in("BEGIN", t2).unwrap();
    let err = e
        .execute_in("UPDATE ub SET v = 30 WHERE id = 1", t2)
        .expect_err("the second same-row writer must would-block");
    assert!(
        matches!(err, EngineError::LockWouldBlock),
        "want LockWouldBlock, got {err:?}"
    );
    e.execute_in("COMMIT", t1).unwrap();
    // The server's retry loop re-runs the statement; emulate one turn.
    e.execute_in("UPDATE ub SET v = 30 WHERE id = 1", t2)
        .unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(one_cell(&mut e, "SELECT v FROM ub"), "30");
}

/// Autocommit challenger against a transaction holder: same contract.
#[test]
fn pin_v7381_autocommit_challenger_would_blocks() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ub2 (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO ub2 VALUES (1, 10)").unwrap();
    let t1 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("UPDATE ub2 SET v = 20 WHERE id = 1", t1)
        .unwrap();
    let err = e
        .execute_in("UPDATE ub2 SET v = 30 WHERE id = 1", IMPLICIT_TX)
        .expect_err("autocommit writer must wait for the tx holder");
    assert!(matches!(err, EngineError::LockWouldBlock), "{err:?}");
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("UPDATE ub2 SET v = 30 WHERE id = 1", IMPLICIT_TX)
        .unwrap();
    assert_eq!(one_cell(&mut e, "SELECT v FROM ub2"), "30");
}

/// DELETE conflicts the same way, and disjoint rows never block.
#[test]
fn pin_v7381_delete_conflicts_and_disjoint_rows_pass() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ub3 (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO ub3 VALUES (1, 10), (2, 20)")
        .unwrap();
    let t1 = e.alloc_tx_id();
    let t2 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("UPDATE ub3 SET v = 11 WHERE id = 1", t1)
        .unwrap();
    e.execute_in("BEGIN", t2).unwrap();
    // Disjoint row: no conflict, no wait.
    e.execute_in("UPDATE ub3 SET v = 22 WHERE id = 2", t2)
        .unwrap();
    // Same row via DELETE: would-block.
    let err = e
        .execute_in("DELETE FROM ub3 WHERE id = 1", t2)
        .expect_err("DELETE against a locked row must would-block");
    assert!(matches!(err, EngineError::LockWouldBlock), "{err:?}");
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("DELETE FROM ub3 WHERE id = 1", t2).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(one_cell(&mut e, "SELECT count(*) FROM ub3"), "1");
    assert_eq!(one_cell(&mut e, "SELECT v FROM ub3"), "22");
}

/// Sequential autocommit writers never block each other (the implicit
/// transaction's locks die with its statement).
#[test]
fn pin_v7381_sequential_autocommit_locks_do_not_linger() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ub4 (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO ub4 VALUES (1, 0)").unwrap();
    for i in 1..=5 {
        e.execute(&format!("UPDATE ub4 SET v = {i} WHERE id = 1"))
            .unwrap();
    }
    assert_eq!(one_cell(&mut e, "SELECT v FROM ub4"), "5");
    assert_eq!(e.locked_row_count(), 0, "no lock may outlive its statement");
}
