//! read01 round 397 (MySQL differential) — `x IS TRUE` / `IS FALSE`
//! coerces a non-boolean to a truth value under the MySQL dialect.
//!
//! MySQL reads any non-zero, non-NULL value as true for `IS TRUE` /
//! `IS FALSE`: `5 IS TRUE` is 1, `0 IS FALSE` is 1, `'abc' IS TRUE` is 0,
//! `'5' IS TRUE` is 1. SPG only matched a real `Bool` operand, so an
//! integer / string / expression operand always tested false — `5 IS TRUE`
//! was false and `0 IS FALSE` was false, a silent-wrong that broke
//! `WHERE col IS TRUE` filtering. A NULL stays not-true / not-false, and a
//! PostgreSQL session keeps its (non-coercing) behavior.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Bool(v) => *v,
            other => panic!("`{sql}` not bool: {other:?}"),
        },
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// A non-zero number is true, zero is false.
#[test]
fn numeric_truth() {
    let mut e = mysql();
    assert!(b(&mut e, "SELECT 5 IS TRUE"));
    assert!(!b(&mut e, "SELECT 0 IS TRUE"));
    assert!(b(&mut e, "SELECT -1 IS TRUE"));
    assert!(b(&mut e, "SELECT 1 IS TRUE"));
}

/// IS FALSE / IS NOT TRUE are the complements.
#[test]
fn is_false_and_negation() {
    let mut e = mysql();
    assert!(!b(&mut e, "SELECT 5 IS FALSE"));
    assert!(b(&mut e, "SELECT 0 IS FALSE"));
    assert!(!b(&mut e, "SELECT 5 IS NOT TRUE"));
    assert!(b(&mut e, "SELECT 0 IS NOT TRUE"));
}

/// A string reads its leading number; NULL is neither true nor false.
#[test]
fn string_and_null() {
    let mut e = mysql();
    assert!(b(&mut e, "SELECT '5' IS TRUE"));
    assert!(!b(&mut e, "SELECT 'abc' IS TRUE"));
    assert!(!b(&mut e, "SELECT '' IS TRUE"));
    assert!(!b(&mut e, "SELECT NULL IS TRUE"));
    assert!(!b(&mut e, "SELECT NULL IS FALSE"));
}

/// A real boolean expression is unaffected.
#[test]
fn boolean_expression_unaffected() {
    let mut e = mysql();
    assert!(b(&mut e, "SELECT (1 > 0) IS TRUE"));
    assert!(b(&mut e, "SELECT (1 < 0) IS FALSE"));
}

/// A `WHERE col IS TRUE` filter now works on an integer column.
#[test]
fn where_is_true_filter() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT, flag INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1,5),(2,0),(3,1)").unwrap();
    match e
        .execute("SELECT id FROM t WHERE flag IS TRUE ORDER BY id")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let ids: Vec<i32> = rows
                .iter()
                .map(|r| match &r.values[0] {
                    Value::Int(n) => *n,
                    o => panic!("{o:?}"),
                })
                .collect();
            assert_eq!(ids, vec![1, 3]);
        }
        other => panic!("{other:?}"),
    }
}

/// A PostgreSQL session keeps its non-coercing behavior (unchanged).
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    // (1>0) IS TRUE is a real boolean and works in both dialects.
    assert!(b(&mut e, "SELECT (1 > 0) IS TRUE"));
}
