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
