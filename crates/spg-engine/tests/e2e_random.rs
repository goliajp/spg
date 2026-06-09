//! PG `random()` — uniform float in [0, 1).
//!
//! SPG's engine is `#![no_std]` and can't reach `std::time::*`
//! or `getrandom`. The PRNG uses a deterministic per-engine
//! seed derived from `Engine`'s already-injected clock. Two
//! calls within one statement return DIFFERENT values
//! (PG-canonical) — `random()` evaluates per-row.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

fn f(v: &Value) -> f64 {
    match v {
        Value::Float(x) => *x,
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn random_returns_value_in_zero_to_one() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT random()").unwrap());
    let v = f(&row[0]);
    assert!(v >= 0.0 && v < 1.0, "got {v}");
}

#[test]
fn random_returns_float_type() {
    let mut e = Engine::new();
    let r = e.execute("SELECT random()").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Float);
}

#[test]
fn random_takes_no_args() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT random(1)").is_err());
}

#[test]
fn random_many_calls_distribute_widely() {
    // Loose distribution check: sample N times and verify the
    // spread covers at least both halves of [0, 1).
    let mut e = Engine::new();
    let mut min = 1.0_f64;
    let mut max = 0.0_f64;
    for _ in 0..50 {
        let row = one_row(e.execute("SELECT random()").unwrap());
        let v = f(&row[0]);
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    assert!(min < 0.5, "min={min} should fall below 0.5 in 50 samples");
    assert!(max > 0.5, "max={max} should rise above 0.5 in 50 samples");
}

#[test]
fn random_in_where_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    for i in 1..=100 {
        e.execute(&format!("INSERT INTO u VALUES ({i})")).unwrap();
    }
    // Roughly half should pass random() < 0.5 (loose check).
    let r = e
        .execute("SELECT count(*) FROM u WHERE random() < 0.5")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    let count = match &rows[0].values[0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        other => panic!("got {other:?}"),
    };
    assert!((10..=90).contains(&count), "expected ~50, got {count}");
}

#[test]
fn random_inside_order_by_for_sampling() {
    // `ORDER BY random()` is the canonical "random row" pick.
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    for i in 1..=10 {
        e.execute(&format!("INSERT INTO u VALUES ({i})")).unwrap();
    }
    let r = e
        .execute("SELECT id FROM u ORDER BY random() LIMIT 1")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    let id = match &rows[0].values[0] {
        Value::Int(n) => *n,
        other => panic!("got {other:?}"),
    };
    assert!((1..=10).contains(&id));
}
