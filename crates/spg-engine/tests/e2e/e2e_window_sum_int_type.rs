//! v7.38 (read01 sweep) — SUM() as a window function over integer input
//! returns BIGINT (like the GROUP BY sum() path and PG), not double. Float and
//! numeric inputs keep their own result types. Oracle from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn window_sum_result_types() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wt (v INT, f FLOAT8, n NUMERIC)").unwrap();
    e.execute("INSERT INTO wt VALUES (10, 1.5, 2.5), (20, 2.5, 3.5)").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(v) OVER () FROM wt LIMIT 1"), Value::BigInt(30));
    assert_eq!(one(&mut e, "SELECT sum(f) OVER () FROM wt LIMIT 1"), Value::Float(4.0));
    assert_eq!(one(&mut e, "SELECT sum(n) OVER () FROM wt LIMIT 1"), Value::Numeric { scaled: 60, scale: 1 , kind: spg_storage::NumericKind::Finite });
    // Running frame sums are also BIGINT.
    match e.execute("SELECT sum(v) OVER (ORDER BY v) FROM wt ORDER BY v").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::BigInt(10));
            assert_eq!(rows[1].values[0], Value::BigInt(30));
        }
        _ => panic!(),
    }
}
