//! v6.4.1 — `GROUP BY ALL` shortcut.
//!
//! Replaces the user-typed GROUP BY column list with every non-
//! aggregate SELECT-list expression. Mirrors DuckDB / PG 19.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn sums_with_group_by_all() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE sales (region TEXT, qty INT)")
        .unwrap();
    for (r, q) in [("east", 10), ("west", 20), ("east", 5), ("west", 30)] {
        eng.execute(&format!("INSERT INTO sales VALUES ('{r}', {q})"))
            .unwrap();
    }
    // GROUP BY ALL means "group by every non-aggregate" — here
    // `region` is the only non-aggregate. Same plan as `GROUP BY
    // region`.
    let res = eng
        .execute("SELECT region, SUM(qty) FROM sales GROUP BY ALL ORDER BY region")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::text("east"), Value::BigInt(15)],
            vec![Value::text("west"), Value::BigInt(50)],
        ]
    );
}

#[test]
fn group_by_all_with_two_non_aggregate_keys() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a TEXT, b TEXT, n INT)")
        .unwrap();
    for (a, b, n) in [
        ("x", "1", 100),
        ("x", "1", 50),
        ("x", "2", 30),
        ("y", "1", 7),
    ] {
        eng.execute(&format!("INSERT INTO t VALUES ('{a}', '{b}', {n})"))
            .unwrap();
    }
    let res = eng
        .execute("SELECT a, b, SUM(n) FROM t GROUP BY ALL ORDER BY a, b")
        .unwrap();
    let got = rows_of(res);
    assert_eq!(
        got,
        vec![
            vec![Value::text("x"), Value::text("1"), Value::BigInt(150),],
            vec![Value::text("x"), Value::text("2"), Value::BigInt(30),],
            vec![Value::text("y"), Value::text("1"), Value::BigInt(7),],
        ]
    );
}

#[test]
fn group_by_all_only_aggregates_yields_single_row() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT)").unwrap();
    for n in [1, 2, 3, 4] {
        eng.execute(&format!("INSERT INTO t VALUES ({n})")).unwrap();
    }
    // No non-aggregate items → GROUP BY ALL expands to empty list,
    // same as plain `SELECT SUM(n) FROM t` (one row covering all).
    let res = eng.execute("SELECT SUM(n) FROM t GROUP BY ALL").unwrap();
    let got = rows_of(res);
    assert_eq!(got, vec![vec![Value::BigInt(10)]]);
}
