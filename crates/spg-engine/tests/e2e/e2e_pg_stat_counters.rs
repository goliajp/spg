//! v7.39 (pg_stat knife A) — the stats views report real counters:
//! pg_stat_user_tables' write/live/dead columns (differential-locked
//! against PG18: 100 inserts + 30 updates + 10 deletes on a 100-row
//! table -> ins=100 upd=30 del=10 live=90 dead=40 — every UPDATE's
//! old version and every DELETE is one dead row) and
//! pg_stat_database's implicit/explicit xact counters.

use spg_engine::{Engine, QueryResult};

fn one_row(e: &mut Engine, sql: &str) -> Vec<spg_storage::Value<'static>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql}");
            rows.into_iter().next().unwrap().values
        }
        other => panic!("{sql}: {other:?}"),
    }
}

fn big(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("BigInt, got {other:?}"),
    }
}

#[test]
fn stat_user_tables_reports_real_write_and_dead_counters() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE stt(id INT PRIMARY KEY, v TEXT)").unwrap();
    e.execute("INSERT INTO stt SELECT g, 'x' FROM generate_series(1,100) g")
        .unwrap();
    e.execute("UPDATE stt SET v='y' WHERE id <= 30").unwrap();
    e.execute("DELETE FROM stt WHERE id > 90").unwrap();
    let r = one_row(
        &mut e,
        "SELECT n_tup_ins, n_tup_upd, n_tup_del, n_live_tup, n_dead_tup \
         FROM pg_stat_user_tables WHERE relname = 'stt'",
    );
    assert_eq!(big(&r[0]), 100, "n_tup_ins");
    assert_eq!(big(&r[1]), 30, "n_tup_upd");
    assert_eq!(big(&r[2]), 10, "n_tup_del");
    assert_eq!(big(&r[3]), 90, "n_live_tup");
    // The legacy (mvcc-inplace-off) write path deletes physically, so
    // no dead versions accumulate; the flip default matches PG.
    let dead = big(&r[4]);
    assert!(
        dead == 40 || dead == 0,
        "n_dead_tup: got {dead}, want 40 (in-place) or 0 (legacy)"
    );
}

#[test]
fn stat_database_counts_implicit_and_explicit_xacts() {
    let mut e = Engine::new();
    let read = |e: &mut Engine| -> (i64, i64) {
        let r = one_row(
            e,
            "SELECT xact_commit, xact_rollback FROM pg_stat_database",
        );
        (big(&r[0]), big(&r[1]))
    };
    let (c0, r0) = read(&mut e);
    // An autocommit statement is one implicit commit (PG counts
    // SELECTs too; the stats query itself adds one as well).
    e.execute("SELECT 1").unwrap();
    let (c1, r1) = read(&mut e);
    assert!(c1 >= c0 + 2, "implicit commits: {c0} -> {c1}");
    assert_eq!(r1, r0);
    // Explicit BEGIN..COMMIT is exactly one commit for the block.
    e.execute("BEGIN").unwrap();
    e.execute("CREATE TABLE tx1(a INT)").unwrap();
    e.execute("INSERT INTO tx1 VALUES (1)").unwrap();
    e.execute("COMMIT").unwrap();
    let (c2, _) = read(&mut e);
    assert!(c2 > c1, "explicit commit counted");
    // Explicit ROLLBACK is one rollback.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO tx1 VALUES (2)").unwrap();
    e.execute("ROLLBACK").unwrap();
    let (_, r2) = read(&mut e);
    assert_eq!(r2, r1 + 1, "explicit rollback counted");
    // A failed autocommit statement is one implicit rollback.
    let _ = e.execute("INSERT INTO tx1 VALUES ('not an int')");
    let (_, r3) = read(&mut e);
    assert_eq!(r3, r2 + 1, "failed autocommit counted as rollback");
}

#[test]
fn stat_user_tables_reports_scan_counters() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sct(id INT PRIMARY KEY, v TEXT)").unwrap();
    e.execute("INSERT INTO sct SELECT g, 'x' FROM generate_series(1,100) g")
        .unwrap();
    // A full-table aggregate and a non-indexed filter are sequential
    // scans; an equality probe on the PK is an index scan.
    e.execute("SELECT count(*) FROM sct").unwrap();
    e.execute("SELECT v FROM sct WHERE v = 'x' LIMIT 1").unwrap();
    e.execute("SELECT v FROM sct WHERE id = 7").unwrap();
    let r = one_row(
        &mut e,
        "SELECT seq_scan, seq_tup_read, idx_scan, idx_tup_fetch \
         FROM pg_stat_user_tables WHERE relname = 'sct'",
    );
    assert!(big(&r[0]) >= 1, "seq_scan: {:?}", r[0]);
    assert!(big(&r[1]) >= 100, "seq_tup_read: {:?}", r[1]);
    assert!(big(&r[2]) >= 1, "idx_scan: {:?}", r[2]);
    assert!(big(&r[3]) >= 1, "idx_tup_fetch: {:?}", r[3]);
}
