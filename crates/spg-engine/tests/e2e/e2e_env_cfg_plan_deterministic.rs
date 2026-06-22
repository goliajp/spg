//! v7.38 P0 元机制 D — `SPG_TEST_PLAN_DETERMINISTIC=1` acceptor.
//! When set, `reorder::reorder_joins_with` is a no-op so the
//! INNER-join chain stays in declared order regardless of
//! per-table statistics. Regression tests that pin "same SQL →
//! same plan" need this so a stray INSERT/ANALYZE doesn't
//! silently flip the join order.

use spg_engine::{Engine, QueryResult, testkit::EnvConfig};

fn build_engine(deterministic: bool) -> Engine {
    let cfg = EnvConfig::builder().plan_deterministic(deterministic).build();
    let mut e = Engine::new().with_env_cfg(cfg);
    // 3-table inner join — production reorder would normally swap
    // the smallest-stats side to be the driving table.
    e.execute("CREATE TABLE small (id INT NOT NULL PRIMARY KEY, k INT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE medium (id INT NOT NULL PRIMARY KEY, k INT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE large (id INT NOT NULL PRIMARY KEY, k INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO small VALUES (1, 1), (2, 2)").unwrap();
    for i in 1..=20 {
        e.execute(&format!("INSERT INTO medium VALUES ({i}, {i})"))
            .unwrap();
    }
    for i in 1..=200 {
        e.execute(&format!("INSERT INTO large VALUES ({i}, {i})"))
            .unwrap();
    }
    e.execute("ANALYZE small").unwrap();
    e.execute("ANALYZE medium").unwrap();
    e.execute("ANALYZE large").unwrap();
    e
}

fn rowcount(r: QueryResult) -> usize {
    match r {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn plan_deterministic_on_and_off_return_same_rows() {
    let sql =
        "SELECT large.id FROM large JOIN medium ON large.k = medium.k JOIN small ON medium.k = small.k ORDER BY large.id";
    let mut e_on = build_engine(true);
    let mut e_off = build_engine(false);
    let on_count = rowcount(e_on.execute(sql).unwrap());
    let off_count = rowcount(e_off.execute(sql).unwrap());
    assert_eq!(
        on_count, off_count,
        "deterministic-on and -off must return the same row set"
    );
    assert_eq!(on_count, 2, "small table has 2 rows that join all three");
}

#[test]
fn plan_deterministic_off_default_in_production() {
    let e = Engine::new();
    assert!(
        !e.env_cfg().plan_deterministic,
        "production default must keep cost-based join reorder on"
    );
}
