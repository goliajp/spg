//! v7.37.7 C.1 — `agg(args) FILTER (WHERE cond)` SQL surface validation.
//!
//! Parser support was added in v7.32 per `parser.rs:9301` comment, but
//! the SPG baseline corpus
//! (`xtests/sqllogictest/corpus/spg_baseline/05_aggregates/filter_clause.test`)
//! still flagged it as "not yet shipped" — likely written before
//! v7.32. This test pins the current behaviour so the corpus marker
//! can be flipped.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: &QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.iter().map(|row| row.values.clone()).collect(),
        _ => panic!("expected Rows"),
    }
}

fn build_engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (g TEXT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES ('a', 10), ('a', 20), ('a', 100), ('b', 5)")
        .unwrap();
    e
}

#[test]
fn sum_filter_where_basic() {
    let mut e = build_engine();
    let r = e
        .execute("SELECT g, SUM(v) FILTER (WHERE v > 10) FROM t GROUP BY g ORDER BY g")
        .expect("FILTER (WHERE ...) parses + executes");
    let got = rows_of(&r);
    assert_eq!(got.len(), 2);
    // Group 'a': v in (10, 20, 100), filter v>10 → 20 + 100 = 120
    assert_eq!(got[0][0], Value::text("a"));
    assert_eq!(got[0][1], Value::BigInt(120));
    // Group 'b': v=5, filter v>10 → SUM over empty set = NULL
    assert_eq!(got[1][0], Value::text("b"));
    assert!(matches!(got[1][1], Value::Null));
}

#[test]
fn count_filter_where_returns_zero_on_empty() {
    let mut e = build_engine();
    let r = e
        .execute("SELECT g, COUNT(*) FILTER (WHERE v > 10) FROM t GROUP BY g ORDER BY g")
        .expect("COUNT(*) FILTER parses");
    let got = rows_of(&r);
    assert_eq!(got.len(), 2);
    // Group 'a': 2 rows match (v=20, v=100)
    assert_eq!(got[0][1], Value::BigInt(2));
    // Group 'b': 0 rows match — COUNT semantics give 0, not NULL
    assert_eq!(got[1][1], Value::BigInt(0));
}

#[test]
fn multiple_filter_clauses_in_one_select() {
    let mut e = build_engine();
    let r = e
        .execute(
            "SELECT g, \
             SUM(v) FILTER (WHERE v > 10), \
             COUNT(*) FILTER (WHERE v < 50) \
             FROM t GROUP BY g ORDER BY g",
        )
        .expect("two FILTER clauses in same SELECT parses");
    let got = rows_of(&r);
    assert_eq!(got.len(), 2);
    // Group 'a': sum-with-filter = 120, count-with-filter (v<50: 10,20) = 2
    assert_eq!(got[0][1], Value::BigInt(120));
    assert_eq!(got[0][2], Value::BigInt(2));
    // Group 'b': sum NULL (no v>10), count = 1 (v=5)
    assert!(matches!(got[1][1], Value::Null));
    assert_eq!(got[1][2], Value::BigInt(1));
}

#[test]
fn filter_clause_with_where_clause_compose() {
    let mut e = build_engine();
    let r = e
        .execute(
            "SELECT g, SUM(v) FILTER (WHERE v > 10) \
             FROM t WHERE g = 'a' GROUP BY g",
        )
        .expect("WHERE + FILTER compose");
    let got = rows_of(&r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][1], Value::BigInt(120));
}
