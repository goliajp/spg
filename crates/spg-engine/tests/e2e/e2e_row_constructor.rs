//! Bare ROW(a, b, …) constructor — PG record-text rendering, plus
//! the ROW keyword joining the paren row-comparison machinery.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn survives(e: &mut Engine, cond: &str) -> bool {
    let sql = alloc_sql(cond);
    let r = e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    !rows.is_empty()
}

fn alloc_sql(cond: &str) -> String {
    format!("SELECT 1 WHERE {cond}")
}

#[test]
fn bare_row_renders_record_text() {
    let mut e = Engine::new();
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT ROW(1, 'a')") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(1,a)");
    // NULL field renders empty; special characters get quoted.
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT ROW(1, NULL, 'x,y')") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(1,,\"x,y\")");
    // Single-element ROW is valid (unlike bare parens).
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT ROW(7)") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(7)");
}

#[test]
fn row_keyword_comparisons() {
    let mut e = Engine::new();
    assert!(survives(&mut e, "ROW(1, 2) = ROW(1, 2)"));
    assert!(!survives(&mut e, "ROW(1, 2) = ROW(1, 3)"));
    assert!(survives(&mut e, "ROW(1, 2) < ROW(1, 3)"));
    assert!(survives(&mut e, "ROW(1, 2) < (2, 0)"));
    assert!(survives(&mut e, "ROW(1, 2) IN ((3, 4), ROW(1, 2))"));
    assert!(!survives(&mut e, "ROW(1, 2) NOT IN ((1, 2))"));
    // Arity mismatch errors.
    assert!(e.execute("SELECT 1 WHERE ROW(1, 2) = ROW(1)").is_err());
}
