//! x op ANY/SOME/ALL (SELECT ...) — quantified subquery
//! comparisons via IN / EXISTS lowering.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    !rows.is_empty()
}

#[test]
fn any_and_some_over_subquery() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE qa (v INT)").unwrap();
    e.execute("INSERT INTO qa VALUES (10), (20)").unwrap();
    // 15 > ANY (10, 20) — true via the 10.
    assert!(b(&mut e, "SELECT 1 WHERE 15 > ANY (SELECT v FROM qa)"));
    assert!(b(&mut e, "SELECT 1 WHERE 15 > SOME (SELECT v FROM qa)"));
    // 5 > ANY — false.
    assert!(!b(&mut e, "SELECT 1 WHERE 5 > ANY (SELECT v FROM qa)"));
    // = ANY is IN.
    assert!(b(&mut e, "SELECT 1 WHERE 20 = ANY (SELECT v FROM qa)"));
    assert!(!b(&mut e, "SELECT 1 WHERE 15 = ANY (SELECT v FROM qa)"));
}

#[test]
fn all_over_subquery() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ql (v INT)").unwrap();
    e.execute("INSERT INTO ql VALUES (10), (20)").unwrap();
    // 25 > ALL (10, 20) — true; 15 > ALL — false via the 20.
    assert!(b(&mut e, "SELECT 1 WHERE 25 > ALL (SELECT v FROM ql)"));
    assert!(!b(&mut e, "SELECT 1 WHERE 15 > ALL (SELECT v FROM ql)"));
    // <> ALL is NOT IN.
    assert!(b(&mut e, "SELECT 1 WHERE 15 <> ALL (SELECT v FROM ql)"));
    assert!(!b(&mut e, "SELECT 1 WHERE 20 <> ALL (SELECT v FROM ql)"));
}

#[test]
fn correlated_outer_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp (id INT, sal INT)").unwrap();
    e.execute("CREATE TABLE cap (limit_v INT)").unwrap();
    e.execute("INSERT INTO emp VALUES (1, 50), (2, 150)")
        .unwrap();
    e.execute("INSERT INTO cap VALUES (100)").unwrap();
    // Per-row: keep employees above every cap.
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT id FROM emp WHERE sal > ALL (SELECT limit_v FROM cap)")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].values[0], spg_storage::Value::Int(2)));
}
