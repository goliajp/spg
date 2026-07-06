//! v7.38 (read01 P3.26) — a statement failing inside an explicit
//! transaction aborts the whole block: later statements are rejected until
//! ROLLBACK / COMMIT, a COMMIT is downgraded to a ROLLBACK, and the tx's
//! work is discarded. Matches live PG 18.4.

use spg_engine::{Engine, EngineError, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn error_in_transaction_aborts_block_and_discards_work() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();

    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    assert!(e.execute("SELECT 1 / 0").is_err()); // the failure

    // Every non-ending statement is now rejected.
    assert!(matches!(
        e.execute("SELECT 42"),
        Err(EngineError::InFailedTransaction)
    ));
    assert!(matches!(
        e.execute("INSERT INTO t VALUES (2)"),
        Err(EngineError::InFailedTransaction)
    ));

    // ROLLBACK is allowed and recovers the session.
    e.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&mut e, "SELECT 42"), 1);
    // The pre-error INSERT was discarded with the aborted tx.
    assert_eq!(rows(&mut e, "SELECT * FROM t"), 0);
}

#[test]
fn commit_in_aborted_tx_rolls_back() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (3)").unwrap();
    assert!(e.execute("SELECT 1 / 0").is_err());
    // PG turns a COMMIT of an aborted tx into a ROLLBACK.
    e.execute("COMMIT").unwrap();
    assert_eq!(rows(&mut e, "SELECT * FROM t"), 0);
}

#[test]
fn autocommit_error_does_not_abort() {
    // An error outside an explicit transaction leaves the session usable.
    let mut e = Engine::new();
    assert!(e.execute("SELECT 1 / 0").is_err());
    assert_eq!(rows(&mut e, "SELECT 42"), 1);
}
