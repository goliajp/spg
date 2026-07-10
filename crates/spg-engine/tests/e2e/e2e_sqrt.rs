//! PG `sqrt(x)` — square root.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

fn float_result(e: &mut Engine, sql: &str) -> f64 {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Float(x) => *x,
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        Value::Numeric { scaled, scale, .. } => (*scaled as f64) / 10f64.powi(i32::from(*scale)),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn sqrt_perfect_square() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT sqrt(16)"), 4.0);
    assert_eq!(float_result(&mut e, "SELECT sqrt(25)"), 5.0);
}

#[test]
fn sqrt_zero() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT sqrt(0)"), 0.0);
}

#[test]
fn sqrt_one() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT sqrt(1)"), 1.0);
}

#[test]
fn sqrt_non_perfect_square() {
    let mut e = Engine::new();
    let r = float_result(&mut e, "SELECT sqrt(2)");
    assert!((r - 1.4142135623730951).abs() < 1e-9);
}

#[test]
fn sqrt_fractional() {
    let mut e = Engine::new();
    let r = float_result(&mut e, "SELECT sqrt(0.25)");
    assert!((r - 0.5).abs() < 1e-9);
}

#[test]
fn sqrt_large_number() {
    let mut e = Engine::new();
    let r = float_result(&mut e, "SELECT sqrt(1000000)");
    assert_eq!(r, 1000.0);
}

#[test]
fn sqrt_negative_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sqrt(-1)").is_err());
}

#[test]
fn sqrt_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT sqrt(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn sqrt_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sqrt()").is_err());
}

#[test]
fn sqrt_two_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sqrt(4, 2)").is_err());
}

#[test]
fn sqrt_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT sqrt('hello')").is_err());
}

#[test]
fn sqrt_pythagorean() {
    let mut e = Engine::new();
    // sqrt(3^2 + 4^2) = 5.
    let r = float_result(&mut e, "SELECT sqrt(power(3, 2) + power(4, 2))");
    assert!((r - 5.0).abs() < 1e-9);
}
