//! v7.39 (read01 round 136 — nested auto-updatable views + CASCADED/LOCAL) —
//! a write through a view whose FROM is itself an updatable view now redirects
//! down the whole chain to the base table (SPG previously errored "relation
//! does not exist" because the redirect stopped at the first view). WITH CHECK
//! OPTION follows PG's cascade rule: the written view's qual is always checked;
//! an underlying view's qual is checked iff the written view is CASCADED or that
//! underlying view itself has a check option. The error names the specific
//! failing view. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn err_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("{sql}: expected error, got Ok"),
    }
}

fn dump(e: &mut Engine) -> Vec<i32> {
    match e.execute("SELECT x FROM t ORDER BY x").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Int(n) => *n,
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn nested_view_insert_update_delete() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(x int)").unwrap();
    e.execute("CREATE VIEW v1 AS SELECT * FROM t WHERE x > 0").unwrap();
    e.execute("CREATE VIEW v2 AS SELECT * FROM v1 WHERE x < 100").unwrap();
    // INSERT/UPDATE/DELETE through the two-level view chain reach the base table.
    e.execute("INSERT INTO v2 VALUES(60)").unwrap();
    assert_eq!(dump(&mut e), vec![60]);
    e.execute("UPDATE v2 SET x=70 WHERE x=60").unwrap();
    assert_eq!(dump(&mut e), vec![70]);
    e.execute("DELETE FROM v2 WHERE x=70").unwrap();
    assert_eq!(dump(&mut e), Vec::<i32>::new());
    // The composed WHERE restricts visibility: a base row outside the chain's
    // predicates is invisible through v2.
    e.execute("INSERT INTO t VALUES(5),(150),(-3),(50)").unwrap();
    match e.execute("SELECT x FROM v2 ORDER BY x").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let xs: Vec<i32> = rows
                .iter()
                .map(|r| match &r.values[0] {
                    spg_storage::Value::Int(n) => *n,
                    o => panic!("{o:?}"),
                })
                .collect();
            assert_eq!(xs, vec![5, 50]); // 0<x<100
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cascaded_checks_both_levels_and_names_failing_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(x int)").unwrap();
    e.execute("CREATE VIEW v1 AS SELECT * FROM t WHERE x > 0").unwrap();
    e.execute("CREATE VIEW v2c AS SELECT * FROM v1 WHERE x < 100 WITH CASCADED CHECK OPTION")
        .unwrap();
    // Violates the underlying v1 (x>0) → error names v1.
    let m = err_of(&mut e, "INSERT INTO v2c VALUES(-5)");
    assert!(m.contains("view \"v1\""), "{m}");
    assert!(m.contains("Failing row contains (-5)"), "{m}");
    // Violates the written v2c (x<100) → error names v2c.
    let m = err_of(&mut e, "INSERT INTO v2c VALUES(200)");
    assert!(m.contains("view \"v2c\""), "{m}");
    // Satisfies both → lands.
    e.execute("INSERT INTO v2c VALUES(80)").unwrap();
    assert_eq!(dump(&mut e), vec![80]);
    // UPDATE through CASCADED to a value violating the underlying → error.
    let m = err_of(&mut e, "UPDATE v2c SET x=-1 WHERE x=80");
    assert!(m.contains("view \"v1\""), "{m}");
}

#[test]
fn local_does_not_cascade_to_uncheck_underlying() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(x int)").unwrap();
    e.execute("CREATE VIEW v1 AS SELECT * FROM t WHERE x > 0").unwrap();
    e.execute("CREATE VIEW v2l AS SELECT * FROM v1 WHERE x < 100 WITH LOCAL CHECK OPTION")
        .unwrap();
    // LOCAL only checks v2l's own qual (x<100). v1 has no check option, so its
    // x>0 is NOT enforced: x=-9 satisfies x<100 and lands.
    e.execute("INSERT INTO v2l VALUES(-9)").unwrap();
    assert_eq!(dump(&mut e), vec![-9]);
    // But LOCAL still rejects a violation of its OWN qual.
    let m = err_of(&mut e, "INSERT INTO v2l VALUES(500)");
    assert!(m.contains("view \"v2l\""), "{m}");
}

#[test]
fn local_cascades_to_check_bearing_underlying() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(x int)").unwrap();
    e.execute("CREATE VIEW v1c AS SELECT * FROM t WHERE x > 0 WITH CHECK OPTION")
        .unwrap();
    e.execute("CREATE VIEW v2ll AS SELECT * FROM v1c WHERE x < 100 WITH LOCAL CHECK OPTION")
        .unwrap();
    // v1c has its own check option, so LOCAL on v2ll still enforces it: x=-5
    // violates v1c's x>0 → error names v1c.
    let m = err_of(&mut e, "INSERT INTO v2ll VALUES(-5)");
    assert!(m.contains("view \"v1c\""), "{m}");
    e.execute("INSERT INTO v2ll VALUES(50)").unwrap();
    assert_eq!(dump(&mut e), vec![50]);
}
