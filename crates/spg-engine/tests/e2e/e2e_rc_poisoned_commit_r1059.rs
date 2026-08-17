//! r1059 — a poisoned READ COMMITTED transaction's COMMIT must not
//! erase DDL committed concurrently by another session.
//!
//! Found by the perm-runner era's sqlx gate flaking ~1-in-6: the pool
//! runs fifteen tests in parallel, and `DROP; CREATE; BEGIN; INSERT`
//! on one connection raced some other connection's in-flight
//! transaction. When that transaction carried a statement that
//! poisons the RC rebase (DDL, MERGE-CTE, …), its COMMIT skipped the
//! rebase AND the round-496 dirty-table merge (gated on
//! `cached_snapshot.is_some()`, i.e. RR/SER only) and installed its
//! whole BEGIN-time shadow — wiping the neighbour's freshly created
//! table ("does not exist") or resurrecting a dropped one ("already
//! exists").

use spg_engine::{Engine, IMPLICIT_TX, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0])
            .parse()
            .unwrap(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Concurrent CREATE TABLE survives a poisoned RC tx's COMMIT.
#[test]
fn poisoned_rc_commit_keeps_concurrent_create() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base (a INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base VALUES (1)", tx).unwrap();
    // Poison the rebase: DDL inside the transaction.
    e.execute_in("CREATE INDEX base_a ON base (a)", tx).unwrap();
    // Another session commits DDL while the tx is open.
    e.execute_in("CREATE TABLE conc (b INT)", IMPLICIT_TX)
        .unwrap();
    e.execute_in("INSERT INTO conc VALUES (7)", IMPLICIT_TX)
        .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    // The tx's own work landed…
    assert_eq!(count(&mut e, "SELECT count(*) FROM base"), 1);
    // …and the neighbour's table must still exist with its row.
    assert_eq!(count(&mut e, "SELECT count(*) FROM conc"), 7 / 7);
}

/// Concurrent DROP TABLE stays dropped through the same COMMIT.
#[test]
fn poisoned_rc_commit_keeps_concurrent_drop() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base2 (a INT)").unwrap();
    e.execute("CREATE TABLE doomed (x INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base2 VALUES (1)", tx).unwrap();
    e.execute_in("CREATE INDEX base2_a ON base2 (a)", tx)
        .unwrap();
    e.execute_in("DROP TABLE doomed", IMPLICIT_TX).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    // The drop must hold: re-creating is legal, selecting is not.
    let err = e
        .execute("SELECT count(*) FROM doomed")
        .expect_err("doomed must stay dropped after the poisoned COMMIT");
    assert!(format!("{err}").contains("doomed"), "{err}");
    e.execute("CREATE TABLE doomed (x INT)").unwrap();
}

/// The second hole behind the same flake: the extended protocol's
/// direct route (`execute_prepared_in_with_cancel`) never bumped
/// `commit_epoch`, so an UN-poisoned RC transaction whose last rebase
/// matched the stale epoch skipped its commit-time rebase and
/// wholesale-installed a shadow that still contained a table the
/// prepared path had just dropped.
#[test]
fn prepared_autocommit_bumps_the_epoch_for_open_txs() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base3 (a INT)").unwrap();
    e.execute("CREATE TABLE doomed3 (x INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    // A plain DML keeps the tx un-poisoned and aligns its rebase
    // epoch with the current one.
    e.execute_in("INSERT INTO base3 VALUES (1)", tx).unwrap();
    // Concurrent autocommit DDL over the PREPARED path (the sqlx /
    // extended-protocol route).
    let stmt = e.prepare("DROP TABLE doomed3").unwrap();
    e.execute_prepared_in_with_cancel(stmt, &[], IMPLICIT_TX, spg_engine::CancelToken::none())
        .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    // The drop must survive the COMMIT.
    let err = e
        .execute("SELECT count(*) FROM doomed3")
        .expect_err("doomed3 must stay dropped after the concurrent COMMIT");
    assert!(format!("{err}").contains("doomed3"), "{err}");
}
