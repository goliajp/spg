//! v7.39 (read01 round 169) — the VACUUM statement does real work.
//! It was a parse-time no-op from the pre-MVCC era ("SPG has no MVCC
//! bloat today"); after the v7.37.15 in-place MVCC flip, tombstoned
//! versions are REAL bloat and a customer's manual `VACUUM [table]`
//! silently reclaimed nothing (probe-revealed in the r168 perf
//! decomposition). Now: `VACUUM t` reclaims the table's dead versions,
//! bare `VACUUM` sweeps every table, `VACUUM ANALYZE` also refreshes
//! statistics, and the option spellings parse. Observable via
//! pg_stat_user_tables.n_dead_tup.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn dead(e: &mut Engine, t: &str) -> i64 {
    one(
        e,
        &format!("SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = '{t}'"),
    )
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE vt(id INT PRIMARY KEY, v INT NOT NULL)")
        .unwrap();
    for i in 0..500 {
        e.execute(&format!("INSERT INTO vt VALUES ({i}, {i})"))
            .unwrap();
    }
}

#[test]
fn vacuum_table_reclaims_dead_versions() {
    let mut e = Engine::new();
    if !e.mvcc_inplace() {
        return; // gate-off: physical delete leaves no tombstones.
    }
    setup(&mut e);
    // Tombstone ~500 versions (below the autovacuum dead>=1000 floor so
    // the statement, not the auto pass, must do the reclaim).
    e.execute("UPDATE vt SET v = v + 1").unwrap();
    assert!(dead(&mut e, "vt") > 0, "update must tombstone");
    e.execute("VACUUM vt").unwrap();
    assert_eq!(dead(&mut e, "vt"), 0, "manual VACUUM must reclaim");
    assert_eq!(one(&mut e, "SELECT count(*) FROM vt"), 500);
}

#[test]
fn bare_vacuum_and_spellings() {
    let mut e = Engine::new();
    if !e.mvcc_inplace() {
        return;
    }
    setup(&mut e);
    e.execute("UPDATE vt SET v = v + 1").unwrap();
    assert!(dead(&mut e, "vt") > 0);
    // Bare VACUUM sweeps all tables.
    e.execute("VACUUM").unwrap();
    assert_eq!(dead(&mut e, "vt"), 0);
    // Option spellings all parse and run.
    e.execute("UPDATE vt SET v = v + 1").unwrap();
    e.execute("VACUUM (FULL, VERBOSE) vt").unwrap();
    assert_eq!(dead(&mut e, "vt"), 0);
    e.execute("UPDATE vt SET v = v + 1").unwrap();
    e.execute("VACUUM ANALYZE vt").unwrap();
    assert_eq!(dead(&mut e, "vt"), 0);
    assert_eq!(one(&mut e, "SELECT count(*) FROM vt"), 500);
}
