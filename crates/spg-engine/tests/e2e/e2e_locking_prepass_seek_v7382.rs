//! 7.38.2 R1 — the locking pre-pass seeks instead of scanning.
//!
//! The 10:40 tpcc profile put `run_locking_prepass` at 64% of the
//! serving thread: every `SELECT … FOR UPDATE` reproduced its row
//! choice by walking EVERY visible row through the interpreted
//! evaluator, while the ordinary execution right after it seeks. The
//! pre-pass now goes through the same index-seek candidate machinery,
//! and `pg_stat_user_tables.seq_scan` is the witness: a point lookup
//! with FOR UPDATE must not sequential-scan the table.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seq_scans(e: &mut Engine) -> i64 {
    one(
        e,
        "SELECT seq_scan FROM pg_stat_user_tables WHERE relname = 'lk'",
    )
    .parse()
    .unwrap()
}

#[test]
fn pin_v7382_point_for_update_does_not_seq_scan() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lk (id INT PRIMARY KEY, v INT)")
        .unwrap();
    for i in 0..500 {
        e.execute(&format!("INSERT INTO lk VALUES ({i}, {})", i * 10))
            .unwrap();
    }
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    let before = seq_scans(&mut e);
    let got = match e
        .execute_in("SELECT v FROM lk WHERE id = 123 FOR UPDATE", tx)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, "1230");
    let after = seq_scans(&mut e);
    assert_eq!(
        after - before,
        0,
        "a point FOR UPDATE walked the whole table ({} extra seq scans) — \
         the locking pre-pass is scanning instead of seeking",
        after - before
    );
    e.execute_in("COMMIT", tx).unwrap();

    // The answers stay right when the pre-pass seeks: a contended row
    // is still locked (second tx would-block), and a non-matching
    // predicate locks nothing.
    let t1 = e.alloc_tx_id();
    let t2 = e.alloc_tx_id();
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("SELECT v FROM lk WHERE id = 7 FOR UPDATE", t1)
        .unwrap();
    e.execute_in("BEGIN", t2).unwrap();
    let err = e
        .execute_in("UPDATE lk SET v = 0 WHERE id = 7", t2)
        .unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::LockWouldBlock),
        "{err:?}"
    );
    e.execute_in("ROLLBACK", t2).unwrap();
    e.execute_in("COMMIT", t1).unwrap();
}
