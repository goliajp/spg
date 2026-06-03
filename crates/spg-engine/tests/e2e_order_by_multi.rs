//! v6.4.0 — multi-column ORDER BY + SELECT-list alias resolution.
//!
//! Pre-v6.4.0 only the first ORDER BY key was honored and aliases
//! defined in the SELECT list weren't visible to ORDER BY. v6.4.0
//! fixes both: every key contributes to the comparator (left-to-
//! right tie-break), and SELECT-list aliases resolve before falling
//! through to the FROM schema.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn two_key_asc_desc_correct() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    // 3 rows of (1, 30), (1, 10), (1, 20), (2, 25), (2, 15) so that
    // ORDER BY a ASC, b DESC must produce a=1 first ordered by b
    // descending, then a=2 ordered by b descending.
    for (a, b) in [(1, 30), (1, 10), (1, 20), (2, 25), (2, 15)] {
        eng.execute(&format!("INSERT INTO t VALUES ({a}, {b})"))
            .unwrap();
    }
    let res = eng
        .execute("SELECT a, b FROM t ORDER BY a ASC, b DESC")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(30)],
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(25)],
            vec![Value::Int(2), Value::Int(15)],
        ]
    );
}

#[test]
fn three_key_with_tied_first_keys() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT, c INT)").unwrap();
    // First two keys all tied; third key decides.
    for (a, b, c) in [(1, 1, 3), (1, 1, 1), (1, 1, 2)] {
        eng.execute(&format!("INSERT INTO t VALUES ({a}, {b}, {c})"))
            .unwrap();
    }
    let res = eng
        .execute("SELECT a, b, c FROM t ORDER BY a, b, c")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Int(1), Value::Int(1), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1), Value::Int(3)],
        ]
    );
}

#[test]
fn alias_resolves_to_projection() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    for (a, b) in [(2, 3), (1, 5), (4, 1)] {
        eng.execute(&format!("INSERT INTO t VALUES ({a}, {b})"))
            .unwrap();
    }
    // ORDER BY references the alias `sum`, which doesn't exist in
    // the FROM schema — pre-v6.4.0 this errored.
    let res = eng
        .execute("SELECT a + b AS sum FROM t ORDER BY sum")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(5)], // 4+1
            vec![Value::Int(5)], // 2+3
            vec![Value::Int(6)], // 1+5
        ]
    );
}

#[test]
fn position_ref_still_works() {
    // Regression: v6.2.4 surface (ORDER BY <position>) must keep
    // working after the v6.4.0 multi-key migration.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    for (a, b) in [(2, 3), (1, 5), (4, 1)] {
        eng.execute(&format!("INSERT INTO t VALUES ({a}, {b})"))
            .unwrap();
    }
    let res = eng.execute("SELECT a, b FROM t ORDER BY 2").unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(4), Value::Int(1)],
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(1), Value::Int(5)],
        ]
    );
}

#[test]
fn mixed_alias_and_position_in_one_order_by() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    for (a, b) in [(2, 3), (2, 1), (1, 5), (1, 5)] {
        eng.execute(&format!("INSERT INTO t VALUES ({a}, {b})"))
            .unwrap();
    }
    // Sort by `a` (alias resolves to projection) then by position
    // 2 (the projected `b` column) DESC. The second key only
    // matters within a=2 (where b values are 3 and 1).
    let res = eng
        .execute("SELECT a, b AS bee FROM t ORDER BY a ASC, 2 DESC")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(5)],
            vec![Value::Int(1), Value::Int(5)],
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );
}
