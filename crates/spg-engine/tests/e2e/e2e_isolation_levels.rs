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
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX)
        .unwrap();
    let expect = if e.mvcc_inplace() { 2 } else { 1 };
    assert_eq!(
        count(&mut e, tx),
        expect,
        "RC sees the concurrent commit on its next statement (gate-on)"
    );
    // The tx's own writes survive the rebase.
    e.execute_in("INSERT INTO t VALUES (3)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (4)", IMPLICIT_TX)
        .unwrap();
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
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX)
        .unwrap();
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
    e.execute_in("CREATE TABLE side (y INT NOT NULL)", tx)
        .unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX)
        .unwrap();
    assert_eq!(
        count(&mut e, tx),
        1,
        "post-DDL the tx is frozen (SI) in both gate modes"
    );
    e.execute_in("ROLLBACK", tx).unwrap();
}

#[test]
fn rc_delete_conflict_is_skipped_like_pg() {
    // 7.38.1 S2.1 rewrite — with tuple locks the concurrent autocommit
    // DELETE now WAITS for the holder (PG RC), and after the holder's
    // COMMIT its retry matches nothing: the row was already deleted.
    // Same end state as PG's blocked-then-chain-followed DELETE.
    let mut e = boot();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", tx).unwrap();
    let err = e
        .execute_in("DELETE FROM t WHERE x = 5", IMPLICIT_TX)
        .unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::LockWouldBlock),
        "concurrent same-row DELETE must wait, got {err:?}"
    );
    e.execute_in("COMMIT", tx).unwrap();
    // The server's retry loop re-runs the statement after the wait.
    let QueryResult::CommandOk { affected, .. } = e
        .execute_in("DELETE FROM t WHERE x = 5", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("CommandOk")
    };
    assert_eq!(affected, 0, "the retried DELETE finds the row already gone");
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
    e.execute_in("INSERT INTO t VALUES (3)", IMPLICIT_TX)
        .unwrap();
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
    // 7.38.1 S2.1 rewrite — frozen-snapshot ordering (see the
    // update-update RR test): the autocommit delete commits first,
    // then the RR tx deletes from its stale view and loses at COMMIT.
    let mut e = boot();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    let _ = e.execute_in("SELECT count(*) FROM t", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", IMPLICIT_TX)
        .unwrap();
    e.execute_in("DELETE FROM t WHERE x = 5", tx).unwrap();
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
    e.execute_in("INSERT INTO u VALUES (7)", IMPLICIT_TX)
        .unwrap();
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
    // 7.38.1 S2.1 rewrite — the delta this test used to RECORD is now
    // CLOSED: the concurrent updater waits on the tuple lock, and its
    // post-commit retry re-evaluates WHERE against the winner's row.
    // `SET x = 20 WHERE x = 1` matches nothing once x became 10 —
    // exactly PG's EvalPlanQual end state (one row, x = 10).
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx).unwrap();
    let err = e
        .execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::LockWouldBlock),
        "{err:?}"
    );
    e.execute_in("COMMIT", tx).unwrap();
    let QueryResult::CommandOk { affected, .. } = e
        .execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("CommandOk")
    };
    assert_eq!(affected, 0, "predicate no longer matches the winner's row");
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT x FROM t ORDER BY x", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1, "exactly one surviving version");
    assert_eq!(
        rows[0].values[0],
        spg_storage::Value::Int(10),
        "PG's EvalPlanQual outcome — the recorded delta is closed"
    );
}

#[test]
fn rr_concurrent_update_update_raises_serialization_failure() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    // 7.38.1 S2.1 rewrite — same-order writers now serialise through
    // the tuple lock, so the 40001 face needs the frozen-snapshot
    // ordering: the RR tx freezes its view FIRST (a read), the
    // autocommit writer lands and commits (no lock conflict — the tx
    // holds nothing yet), and the tx's own UPDATE then works on a row
    // the base has already replaced. First-committer-wins: 40001 at
    // COMMIT, exactly PG's RR contract.
    let mut e = boot();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    // Freeze the snapshot before anyone else moves.
    let _ = e.execute_in("SELECT count(*) FROM t", tx).unwrap();
    e.execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap();
    e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx).unwrap();
    let err = e.execute_in("COMMIT", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "RR update-update is first-committer-wins with 40001, got {err:?}"
    );
    let QueryResult::Rows { rows, .. } = e.execute_in("SELECT x FROM t", IMPLICIT_TX).unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(20));
}

// ── v7.37.17 Phase E4 round 2 — savepoint/multi-table/FK/reinsert ──

#[test]
fn savepoint_rollback_interleaves_with_rebase() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1)", tx).unwrap();
    e.execute_in("SAVEPOINT s", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (100)", IMPLICIT_TX)
        .unwrap();
    assert_eq!(count(&mut e, tx), 3, "own 1,2 + concurrent 100");
    e.execute_in("ROLLBACK TO SAVEPOINT s", tx).unwrap();
    assert_eq!(
        count(&mut e, tx),
        2,
        "post-rollback: 1 kept, 2 undone, 100 still visible via rebase"
    );
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(count(&mut e, IMPLICIT_TX), 2, "committed = 1 and 100");
}

#[test]
fn fk_child_insert_vs_concurrent_parent_delete_keeps_integrity() {
    // E4 matrix catch #2: the tx's child insert is invisible to the
    // base, so a concurrent parent DELETE passes its reverse-FK check;
    // the tx's insert-time FK check passed too (parent alive in its
    // view). Without the commit-time FK re-validation the commit
    // installed an ORPHAN child. PG serializes via FOR KEY SHARE and
    // fails the DELETE (23503, child wins); SPG can't retroactively
    // fail a committed autocommit DELETE, so the tx loses with 40001
    // (first-committer-wins) and integrity holds either way.
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE c (pid INT REFERENCES p(id))")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO c VALUES (1)", tx).unwrap();
    e.execute_in("DELETE FROM p WHERE id = 1", IMPLICIT_TX)
        .unwrap();
    let err = e.execute_in("COMMIT", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "orphaning commit must fail with 40001, got {err:?}"
    );
    let QueryResult::Rows { rows, .. } = e.execute_in("SELECT pid FROM c", IMPLICIT_TX).unwrap()
    else {
        panic!("rows")
    };
    assert!(rows.is_empty(), "no orphan child row");
}

#[test]
fn multi_table_tx_rebases_every_touched_table() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (x INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE b (y INT NOT NULL)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO a VALUES (1)", tx).unwrap();
    e.execute_in("INSERT INTO b VALUES (10)", tx).unwrap();
    e.execute_in("INSERT INTO a VALUES (2)", IMPLICIT_TX)
        .unwrap();
    e.execute_in("INSERT INTO b VALUES (20)", IMPLICIT_TX)
        .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    let n = |e: &mut Engine, sql: &str| -> usize {
        match e.execute_in(sql, IMPLICIT_TX) {
            Ok(QueryResult::Rows { rows, .. }) => rows.len(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(n(&mut e, "SELECT x FROM a"), 2, "a keeps both writers");
    assert_eq!(n(&mut e, "SELECT y FROM b"), 2, "b keeps both writers");
}

#[test]
fn delete_then_reinsert_same_pk_survives_concurrent_delete() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    // 7.38.1 S2.1 rewrite — the concurrent DELETE now waits on the
    // tuple lock and retries after the COMMIT. Recorded delta: PG's
    // blocked DELETE follows the locked tuple's UPDATE CHAIN, and a
    // delete+reinsert is not a chain, so PG's retry affects 0 rows and
    // the reinserted row SURVIVES; SPG's server-level retry re-runs
    // the whole statement against the fresh base, so the reinserted
    // row matches and is deleted. Statement-retry vs EvalPlanQual
    // chain-following — ledgered in the 7.38.1 checklist (S2 notes).
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE id = 1", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1, 11)", tx).unwrap();
    let err = e
        .execute_in("DELETE FROM t WHERE id = 1", IMPLICIT_TX)
        .unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::LockWouldBlock),
        "{err:?}"
    );
    e.execute_in("COMMIT", tx).unwrap();
    // Retry after the wait: the fresh statement sees the reinserted
    // row (the recorded delta vs PG's chain-following).
    let QueryResult::CommandOk { affected, .. } = e
        .execute_in("DELETE FROM t WHERE id = 1", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("CommandOk")
    };
    assert_eq!(affected, 1, "statement retry matches the reinserted row");
    let QueryResult::Rows { rows, .. } = e.execute_in("SELECT id, v FROM t", IMPLICIT_TX).unwrap()
    else {
        panic!("rows")
    };
    assert!(
        rows.is_empty(),
        "SPG statement-retry deletes it (delta noted)"
    );
}

// ── v7.37.17 Phase E4 round 3 — unique-key collisions under rebase ──

#[test]
fn rc_insert_insert_unique_collision_raises_40001() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT UNIQUE)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO u VALUES (7)", tx).unwrap();
    e.execute_in("INSERT INTO u VALUES (7)", IMPLICIT_TX)
        .unwrap();
    // The next tx statement rebases and hits the taken key.
    let err = e.execute_in("SELECT count(*) FROM u", tx).unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::SerializationFailure(_)),
        "insert-insert collision -> 40001, got {err:?}"
    );
    e.execute_in("ROLLBACK", tx).unwrap();
    let QueryResult::Rows { rows, .. } = e.execute_in("SELECT x FROM u", IMPLICIT_TX).unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1, "exactly one 7 — no duplicate installed");
}

#[test]
fn on_conflict_vs_concurrent_on_conflict_stays_single_row() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT PRIMARY KEY, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 0)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in(
        "INSERT INTO u VALUES (1, 0) ON CONFLICT (x) DO UPDATE SET n = u.n + 10",
        tx,
    )
    .unwrap();
    e.execute_in(
        "INSERT INTO u VALUES (1, 0) ON CONFLICT (x) DO UPDATE SET n = u.n + 100",
        IMPLICIT_TX,
    )
    .unwrap();
    // With update-pair atomicity the tx's ON CONFLICT (which is an
    // UPDATE against the old version) loses silently under RC —
    // first-committer-wins, exactly like plain update-update. PG
    // waits on the row lock and applies both (n = 110); SPG keeps
    // the winner (n = 100) — recorded delta, retry converges.
    e.execute_in("COMMIT", tx)
        .expect("RC on-conflict loser commits cleanly (its update skipped)");
    let QueryResult::Rows { rows, .. } = e.execute_in("SELECT x, n FROM u", IMPLICIT_TX).unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1, "exactly one row — no duplicate");
    assert_eq!(rows[0].values[1], spg_storage::Value::Int(100));
}

#[test]
fn rr_plain_update_commit_does_not_false_conflict() {
    // Regression guard for the two-phase merge: an UPDATE's new
    // version must not unique-collide with its OWN old version
    // (tombstoned in phase 1 before the check).
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT PRIMARY KEY, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 0)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("UPDATE u SET n = 5 WHERE x = 1", tx).unwrap();
    e.execute_in("COMMIT", tx)
        .expect("uncontended RR UPDATE commit must succeed");
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT n FROM u WHERE x = 1", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(5));
}

// ── v7.37.17 Phase E4 round 4 — trigger / RETURNING / savepoint × rebase ──

#[test]
fn trigger_writes_to_second_table_survive_rebase() {
    // The trigger's embedded INSERT dispatches through
    // execute_stmt_with_cancel, so `log` lands in touched_tables and
    // its write-set replays across the rebase like any direct write.
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE main (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE log (id INT NOT NULL)").unwrap();
    e.execute(
        "CREATE FUNCTION audit() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO log VALUES (NEW.id);
  RETURN NEW;
END;
$$",
    )
    .unwrap();
    e.execute("CREATE TRIGGER tg AFTER INSERT ON main FOR EACH ROW EXECUTE FUNCTION audit()")
        .unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO main VALUES (1)", tx).unwrap();
    e.execute_in("INSERT INTO main VALUES (2)", IMPLICIT_TX)
        .unwrap();
    // Rebase happens on this statement, then commit merges.
    e.execute_in("SELECT count(*) FROM main", tx).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    for (table, ctx) in [("main", "main"), ("log", "trigger-written log")] {
        let QueryResult::Rows { rows, .. } = e
            .execute_in(&format!("SELECT id FROM {table} ORDER BY id"), IMPLICIT_TX)
            .unwrap()
        else {
            panic!("rows")
        };
        let got: Vec<_> = rows.iter().map(|r| r.values[0].clone()).collect();
        assert_eq!(
            got,
            vec![spg_storage::Value::Int(1), spg_storage::Value::Int(2)],
            "{ctx} must hold both the tx row and the concurrent row"
        );
    }
}

#[test]
fn update_returning_survives_rebase() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute_in("UPDATE t SET n = n + 1 WHERE id = 1 RETURNING n", tx)
        .unwrap()
    else {
        panic!("RETURNING rows")
    };
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(11));
    e.execute_in("UPDATE t SET n = n + 100 WHERE id = 2", IMPLICIT_TX)
        .unwrap();
    e.execute_in("SELECT count(*) FROM t", tx).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT n FROM t ORDER BY id", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    let got: Vec<_> = rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        got,
        vec![spg_storage::Value::Int(11), spg_storage::Value::Int(120)],
        "both the RETURNING update and the concurrent update must land"
    );
}

#[test]
fn rr_savepoint_rollback_excludes_rows_from_merge() {
    if !Engine::new().mvcc_inplace() {
        return;
    }
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("UPDATE t SET n = 11 WHERE id = 1", tx)
        .unwrap();
    e.execute_in("SAVEPOINT s1", tx).unwrap();
    e.execute_in("UPDATE t SET n = 22 WHERE id = 2", tx)
        .unwrap();
    e.execute_in("ROLLBACK TO SAVEPOINT s1", tx).unwrap();
    e.execute_in("UPDATE t SET n = 33 WHERE id = 3", IMPLICIT_TX)
        .unwrap();
    e.execute_in("COMMIT", tx)
        .expect("no overlap with the concurrent write — merge must pass");
    let QueryResult::Rows { rows, .. } = e
        .execute_in("SELECT n FROM t ORDER BY id", IMPLICIT_TX)
        .unwrap()
    else {
        panic!("rows")
    };
    let got: Vec<_> = rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        got,
        vec![
            spg_storage::Value::Int(11),
            spg_storage::Value::Int(20),
            spg_storage::Value::Int(33)
        ],
        "kept update lands, savepoint-rolled-back update does not, concurrent write survives"
    );
}
