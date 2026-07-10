//! v7.38 (read01 U3) — `(a, b, …) [NOT] IN (SELECT x, y, …)`: a row
//! constructor tested against a multi-column subquery, with PG's row
//! three-valued logic. Row-vs-list decomposes at parse time; the subquery
//! form survives as an `Expr::RowInSubquery` node. All expected results
//! are live-PG18.4-verified.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}
fn b(e: &mut Engine, sql: &str) -> Option<bool> {
    match scalar(e, sql) {
        Value::Bool(v) => Some(v),
        Value::Null => None,
        o => panic!("{sql}: expected bool/null, got {o:?}"),
    }
}

#[test]
fn row_in_uncorrelated_subquery() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, "SELECT (1,2) IN (SELECT 1,2)"), Some(true));
    assert_eq!(b(&mut e, "SELECT (1,2) IN (SELECT 3,4)"), Some(false));
    assert_eq!(b(&mut e, "SELECT (1,2) NOT IN (SELECT 3,4)"), Some(true));
    assert_eq!(b(&mut e, "SELECT (1,2) NOT IN (SELECT 1,2)"), Some(false));
}

#[test]
fn row_in_subquery_over_values() {
    let mut e = Engine::new();
    let src = "(SELECT x,y FROM (VALUES(1,'a'),(2,'b')) t(x,y))";
    assert_eq!(b(&mut e, &format!("SELECT (2,'b') IN {src}")), Some(true));
    assert_eq!(b(&mut e, &format!("SELECT (9,'z') IN {src}")), Some(false));
}

#[test]
fn row_in_subquery_null_three_valued() {
    // A NULL element makes an otherwise-matching row UNKNOWN (NULL), but a
    // definite mismatch on a non-NULL column is still FALSE.
    let mut e = Engine::new();
    assert_eq!(b(&mut e, "SELECT (1,NULL) IN (SELECT 1,2)"), None); // UNKNOWN
    assert_eq!(b(&mut e, "SELECT (1,NULL) IN (SELECT 3,4)"), Some(false)); // 1<>3
}

#[test]
fn row_in_subquery_in_where_clause() {
    let mut e = Engine::new();
    let n = scalar(
        &mut e,
        "SELECT count(*)::int FROM (VALUES(1,2),(3,4),(5,6)) t(a,b) \
         WHERE (a,b) IN (SELECT 1,2 UNION SELECT 5,6)",
    );
    assert_eq!(n, Value::Int(2));
}

#[test]
fn row_in_correlated_subquery() {
    let mut e = Engine::new();
    let n = scalar(
        &mut e,
        "SELECT count(*)::int FROM (VALUES(1,2),(3,9)) t(a,b) \
         WHERE (a,b) IN (SELECT x,y FROM (VALUES(1,2),(3,4)) s(x,y))",
    );
    assert_eq!(n, Value::Int(1)); // only (1,2) matches
}

#[test]
fn row_in_subquery_arity_mismatch_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT (1,2) IN (SELECT 1,2,3)").is_err());
}
