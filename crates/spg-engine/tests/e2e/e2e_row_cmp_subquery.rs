//! v7.38 (read01 U4) — `(a, b, …) <op> (SELECT x, y, …)`: a row
//! constructor compared to a single-row subquery (`=`, `<>`, `<`, `<=`,
//! `>`, `>=`). Row-vs-literal-row decomposes at parse time; the subquery
//! form survives as `Expr::RowCmpSubquery`. All results live-PG18.4-verified.

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
        o => panic!("{sql}: {o:?}"),
    }
}

#[test]
fn row_eq_and_ne_subquery() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, "SELECT (1,2) = (SELECT 1,2)"), Some(true));
    assert_eq!(b(&mut e, "SELECT (1,2) = (SELECT 3,4)"), Some(false));
    assert_eq!(b(&mut e, "SELECT (1,2) <> (SELECT 1,2)"), Some(false));
    assert_eq!(b(&mut e, "SELECT (1,2) <> (SELECT 1,3)"), Some(true));
}

#[test]
fn row_ordering_subquery_is_lexicographic() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, "SELECT (1,2) < (SELECT 1,3)"), Some(true)); // tie a, b<
    assert_eq!(b(&mut e, "SELECT (1,2) < (SELECT 1,2)"), Some(false)); // equal
    assert_eq!(b(&mut e, "SELECT (1,2) < (SELECT 2,0)"), Some(true)); // a<
    assert_eq!(b(&mut e, "SELECT (2,2) >= (SELECT 2,1)"), Some(true));
    assert_eq!(b(&mut e, "SELECT (1,2) <= (SELECT 1,2)"), Some(true));
}

#[test]
fn row_cmp_empty_subquery_is_null() {
    let mut e = Engine::new();
    // Scalar-subquery rule: no rows → NULL.
    assert_eq!(b(&mut e, "SELECT (1,2) = (SELECT 1,2 WHERE false)"), None);
}

#[test]
fn row_cmp_null_element_is_unknown() {
    let mut e = Engine::new();
    assert_eq!(b(&mut e, "SELECT (1,NULL) = (SELECT 1,2)"), None); // UNKNOWN
    // A definite mismatch on a non-NULL column is still FALSE.
    assert_eq!(b(&mut e, "SELECT (9,NULL) = (SELECT 1,2)"), Some(false));
}

#[test]
fn row_cmp_more_than_one_row_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT (1,2) = (SELECT * FROM (VALUES(1,2),(3,4)) t)")
            .is_err()
    );
}

#[test]
fn row_cmp_in_where_and_correlated() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*)::int FROM (VALUES(1,2),(3,4)) t(a,b) WHERE (a,b) = (SELECT 3,4)",
        ),
        Value::Int(1)
    );
    // Correlated: the subquery references the outer row via a qualified
    // column. Row (1,2) matches its own (1,2); (3,9) sees (3,4) → no match.
    e.execute("CREATE TABLE t_corr(a int, b int)").unwrap();
    e.execute("INSERT INTO t_corr VALUES(1,2),(3,9)").unwrap();
    e.execute("CREATE TABLE s_corr(x int, y int)").unwrap();
    e.execute("INSERT INTO s_corr VALUES(1,2),(3,4)").unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*)::int FROM t_corr \
             WHERE (a,b) = (SELECT x,y FROM s_corr WHERE s_corr.x = t_corr.a)",
        ),
        Value::Int(1)
    );
}
