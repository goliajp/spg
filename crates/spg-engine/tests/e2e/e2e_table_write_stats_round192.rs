//! v7.39 (read01 round 192, B24) — pg_stat_user_tables n_tup_ins /
//! n_tup_upd / n_tup_del are real, engine-side and non-transactional.
//!
//! The counters were bumped on the CATALOG table object, so a bump
//! made inside a transaction landed on the tx's shadow table and the
//! RC rebase rebuilt the shadow from the committed base — the count
//! silently vanished (server-side DML always runs tx-wrapped via the
//! commit barrier, so the wire probe read all-zero). Now they live in
//! an engine-side map, matching PG's stats collector semantics:
//! non-transactional (a rolled-back INSERT still counts), preserved
//! across RENAME, reset by DROP.

use spg_engine::{Engine, QueryResult};

fn stats(e: &mut Engine, table: &str) -> (i64, i64, i64) {
    match e
        .execute(&format!(
            "SELECT n_tup_ins, n_tup_upd, n_tup_del FROM pg_stat_user_tables \
             WHERE relname = '{table}'"
        ))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let g = |i: usize| match rows[0].values[i] {
                spg_storage::Value::BigInt(n) => n,
                ref o => panic!("{o:?}"),
            };
            (g(0), g(1), g(2))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn autocommit_counts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO t SELECT g, g FROM generate_series(1,100) g")
        .unwrap();
    e.execute("UPDATE t SET v = 0 WHERE id <= 10").unwrap();
    e.execute("DELETE FROM t WHERE id > 95").unwrap();
    assert_eq!(stats(&mut e, "t"), (100, 10, 5));
}

#[test]
fn tx_wrapped_counts_survive_commit() {
    // The server leader wraps every autocommit write in BEGIN..COMMIT;
    // pre-r192 these bumps vanished in the RC rebase.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1, 0)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (2, 0)", tx).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(stats(&mut e, "t").0, 2, "tx-wrapped inserts must count");
}

#[test]
fn rolled_back_writes_still_count() {
    // PG's stats collector is non-transactional: a rolled-back INSERT
    // still increments n_tup_ins.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1, 0)", tx).unwrap();
    e.execute_in("ROLLBACK", tx).unwrap();
    assert_eq!(
        stats(&mut e, "t").0,
        1,
        "rolled-back insert still counts (PG collector semantics)"
    );
    // And the row itself is gone.
    match e.execute("SELECT count(*) FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(format!("{:?}", rows[0].values[0]), "BigInt(0)");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rename_keeps_and_drop_resets() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 0)").unwrap();
    e.execute("ALTER TABLE t RENAME TO t2").unwrap();
    assert_eq!(stats(&mut e, "t2").0, 1, "stats survive rename");
    e.execute("DROP TABLE t2").unwrap();
    e.execute("CREATE TABLE t2 (id INT PRIMARY KEY, v INT)").unwrap();
    assert_eq!(stats(&mut e, "t2").0, 0, "recreated table starts at zero");
}
