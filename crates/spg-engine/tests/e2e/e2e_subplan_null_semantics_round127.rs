//! v7.39 (read01 round 127, Track A — nodeSubplan.c 补读) — subquery three-valued
//! logic (ANY / ALL / IN / NOT IN with NULL, empty sets, scalar cardinality),
//! locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/nodeSubplan.c`: no SPG divergence.
//! Pins lock the subtle cases — NULL in the subquery makes `= ANY` / `<> ALL` /
//! `NOT IN` return NULL rather than true/false, an empty set makes ANY false and
//! ALL true, a scalar subquery of 0 rows is NULL, and >1 row errors.

use spg_engine::{Engine, QueryResult};

fn tri(e: &mut Engine, sql: &str) -> String {
    let wrapped = format!("SELECT coalesce(({sql})::text, 'NULL')");
    match e
        .execute(&wrapped)
        .unwrap_or_else(|x| panic!("{wrapped}: {x:?}"))
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{wrapped}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE s (x int)").unwrap();
    e.execute("INSERT INTO s VALUES (1),(2),(NULL)").unwrap();
}

#[test]
fn any_all_not_in_with_null_are_three_valued() {
    let mut e = Engine::new();
    setup(&mut e);
    // 3 not among {1,2} but NULL present → unknown.
    assert_eq!(tri(&mut e, "3 = ANY(SELECT x FROM s)"), "NULL");
    assert_eq!(tri(&mut e, "3 <> ALL(SELECT x FROM s)"), "NULL");
    // The classic NOT IN with a NULL in the subquery.
    assert_eq!(tri(&mut e, "5 NOT IN (SELECT x FROM s)"), "NULL");
    // A definite membership still resolves NOT IN to false.
    assert_eq!(tri(&mut e, "1 NOT IN (SELECT x FROM s)"), "false");
}

#[test]
fn empty_subquery_any_false_all_true() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(tri(&mut e, "9 = ANY(SELECT x FROM s WHERE x>100)"), "false");
    assert_eq!(tri(&mut e, "9 <> ALL(SELECT x FROM s WHERE x>100)"), "true");
}

#[test]
fn scalar_subquery_cardinality() {
    let mut e = Engine::new();
    setup(&mut e);
    // 0 rows → NULL.
    assert_eq!(tri(&mut e, "SELECT x FROM s WHERE x>100"), "NULL");
    // >1 row → error.
    assert!(e.execute("SELECT (SELECT x FROM s)").is_err());
}

#[test]
fn exists_counts_null_rows() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        tri(&mut e, "EXISTS(SELECT 1 FROM s WHERE x IS NULL)"),
        "true"
    );
    assert_eq!(tri(&mut e, "NOT EXISTS(SELECT 1 FROM s WHERE x=9)"), "true");
}
