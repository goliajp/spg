//! v7.37.17 (Phase E2) — isolation-level semantics under the in-place
//! MVCC gate. PG18 anchors: READ COMMITTED (the default) sees rows
//! committed before each STATEMENT; REPEATABLE READ / SERIALIZABLE
//! keep their first-established view. The legacy gate-off matrix
//! (mvcc-inplace-off) keeps the pre-E2 frozen-SI behaviour for every
//! level — writes there carry no version stamps, so the rebase
//! deliberately stands down.

use spg_engine::{Engine, IMPLICIT_TX, QueryResult};

fn count(e: &mut Engine, tx: spg_engine::TxId) -> i64 {
    match e.execute_in("SELECT count(*) FROM t", tx) {
        Ok(QueryResult::Rows { rows, .. }) => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            ref other => panic!("count: {other:?}"),
        },
        other => panic!("count: {other:?}"),
    }
}

fn boot() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e
}

#[test]
fn read_committed_sees_concurrent_commits_per_statement() {
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    assert_eq!(count(&mut e, tx), 1);
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX).unwrap();
    let expect = if e.mvcc_inplace() { 2 } else { 1 };
    assert_eq!(
        count(&mut e, tx),
        expect,
        "RC sees the concurrent commit on its next statement (gate-on)"
    );
    // The tx's own writes survive the rebase.
    e.execute_in("INSERT INTO t VALUES (3)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (4)", IMPLICIT_TX).unwrap();
    let expect = if e.mvcc_inplace() { 4 } else { 2 };
    assert_eq!(
        count(&mut e, tx),
        expect,
        "own write + both concurrent commits visible"
    );
    e.execute_in("COMMIT", tx).unwrap();
    // Gate-on: the pre-COMMIT rebase folds every concurrent commit in
    // before the shadow installs — nothing is lost. Legacy gate-off:
    // COMMIT replaces the whole catalog with the frozen shadow, so the
    // two concurrent autocommit inserts are overwritten — the honest
    // pre-E2 lost-update behaviour, pinned as-is (E3/route-β territory).
    let total = if e.mvcc_inplace() { 4 } else { 2 };
    assert_eq!(count(&mut e, IMPLICIT_TX), total);
}

#[test]
fn repeatable_read_keeps_its_first_view() {
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    assert_eq!(count(&mut e, tx), 1);
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX).unwrap();
    assert_eq!(
        count(&mut e, tx),
        1,
        "RR keeps the view it established, in both gate modes"
    );
    e.execute_in("ROLLBACK", tx).unwrap();
}

#[test]
fn rc_rebase_stands_down_after_ddl() {
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    assert_eq!(count(&mut e, tx), 1);
    // DDL inside the tx poisons the rebase — the tx keeps its frozen
    // view from here on (never risks losing the DDL's shadow effect).
    e.execute_in("CREATE TABLE side (y INT NOT NULL)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX).unwrap();
    assert_eq!(
        count(&mut e, tx),
        1,
        "post-DDL the tx is frozen (SI) in both gate modes"
    );
    e.execute_in("ROLLBACK", tx).unwrap();
}

#[test]
fn rc_delete_conflict_is_skipped_like_pg() {
    // tx deletes a row; a concurrent autocommit delete removes it
    // first (commits immediately). PG RC: the tx's delete simply
    // applies to nothing; COMMIT succeeds; the row stays gone.
    let mut e = boot();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", IMPLICIT_TX).unwrap();
    // Next statement rebases; the conflicting tombstone is skipped.
    assert_eq!(count(&mut e, tx), if e.mvcc_inplace() { 1 } else { 1 });
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(count(&mut e, IMPLICIT_TX), 1, "row deleted exactly once");
}

// ── v7.37.17 Phase E3 — RR/SER commit-merge + serialization failure ──

#[test]
fn rr_commit_merges_concurrent_inserts_instead_of_overwriting() {
    if !Engine::new().mvcc_inplace() {
        return; // legacy matrix keeps the wholesale-install behaviour
    }
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", tx).unwrap();
    // Concurrent autocommit insert AFTER the tx's last statement —
    // the old wholesale install would have silently dropped it.
    e.execute_in("INSERT INTO t VALUES (3)", IMPLICIT_TX).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(
        count(&mut e, IMPLICIT_TX),
        3,
        "RR commit merges its write-set onto the latest base"
    );
}

#[test]
fn rr_commit_conflict_raises_serialization_failure() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = boot();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", tx).unwrap();
    // A concurrent committed delete wins (first-committer-wins).
    e.execute_in("DELETE FROM t WHERE x = 5", IMPLICIT_TX).unwrap();
    let err = e.execute_in("COMMIT", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "expected 40001, got {err:?}"
    );
    assert!(!e.is_tx_open(tx), "failed COMMIT ends the transaction");
    assert_eq!(count(&mut e, IMPLICIT_TX), 1, "base state intact");
}

#[test]
fn rr_commit_unique_conflict_raises_serialization_failure() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT UNIQUE)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("INSERT INTO u VALUES (7)", tx).unwrap();
    // Concurrent committed insert takes the key first.
    e.execute_in("INSERT INTO u VALUES (7)", IMPLICIT_TX).unwrap();
    let err = e.execute_in("COMMIT", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "expected 40001 on duplicate-key merge, got {err:?}"
    );
}

#[test]
fn set_transaction_after_first_query_errors() {
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    assert_eq!(count(&mut e, tx), 1); // first query
    let err = e
        .execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap_err();
    assert!(
        format!("{err}").contains("must be called before any query"),
        "PG 25001 shape, got {err:?}"
    );
    e.execute_in("ROLLBACK", tx).unwrap();
}

// ── v7.37.17 Phase E4 — isolation scenario matrix additions ──

#[test]
fn rc_concurrent_update_update_keeps_one_row() {
    // E4 matrix catch: the tx's UPDATE write-set is tombstone(old) +
    // insert(new); before the pairing fix a conflicting tombstone was
    // skipped while the paired insert still replayed — DUPLICATING the
    // row. SPG resolves update-update as first-committer-wins (the
    // tx's UPDATE ends up matching zero rows). Recorded delta: PG's
    // EvalPlanQual re-applies the loser's update to the winner's row
    // (one row, x=10); SPG keeps the winner (one row, x=20).
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx).unwrap();
    e.execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap();
    assert_eq!(count(&mut e, tx), 1, "no duplicated row after rebase");
    e.execute_in("COMMIT", tx).unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT x FROM t ORDER BY x", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1, "exactly one surviving version");
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(20));
}

#[test]
fn rr_concurrent_update_update_raises_serialization_failure() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx).unwrap();
    e.execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap();
    let err = e.execute_in("COMMIT", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "RR update-update is first-committer-wins with 40001, got {err:?}"
    );
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT x FROM t", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(20));
}
