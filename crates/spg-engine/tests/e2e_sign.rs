//! PG `sign(x)` — -1 / 0 / 1 depending on sign of x.

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

fn val_as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(x) => *x,
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        Value::SmallInt(n) => f64::from(*n),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn sign_positive_returns_one() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(5)").unwrap());
    assert_eq!(val_as_f64(&row[0]), 1.0);
}

#[test]
fn sign_negative_returns_neg_one() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(-5)").unwrap());
    assert_eq!(val_as_f64(&row[0]), -1.0);
}

#[test]
fn sign_zero_returns_zero() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(0)").unwrap());
    assert_eq!(val_as_f64(&row[0]), 0.0);
}

#[test]
fn sign_small_positive_float() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(0.001)").unwrap());
    assert_eq!(val_as_f64(&row[0]), 1.0);
}

#[test]
fn sign_small_negative_float() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(-0.001)").unwrap());
    assert_eq!(val_as_f64(&row[0]), -1.0);
}

#[test]
fn sign_large_negative() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(-9999999999)").unwrap());
    assert_eq!(val_as_f64(&row[0]), -1.0);
}

#[test]
fn sign_zero_float() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(0.0)").unwrap());
    assert_eq!(val_as_f64(&row[0]), 0.0);
}

#[test]
fn sign_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sign(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn sign_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sign()").is_err());
}

#[test]
fn sign_two_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sign(5, 3)").is_err());
}

#[test]
fn sign_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sign('hello')").is_err());
}

#[test]
fn sign_for_branch_logic() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bal (id INT NOT NULL, amount FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO bal VALUES (1, 100.0), (2, -50.0), (3, 0.0)")
        .unwrap();
    let r = e
        .execute("SELECT id, sign(amount) FROM bal ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(val_as_f64(&rows[0].values[1]), 1.0);
    assert_eq!(val_as_f64(&rows[1].values[1]), -1.0);
    assert_eq!(val_as_f64(&rows[2].values[1]), 0.0);
}
