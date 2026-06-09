//! PG `trunc(x)` / `trunc(x, scale)` — truncate toward zero.
//!
//! Distinct from floor() and round():
//!   trunc(1.7)  → 1
//!   trunc(-1.7) → -1   (toward zero, not -2)
//!   floor(-1.7) → -2   (toward -inf)
//!
//! `trunc(x, scale)` truncates to N decimal places.

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

fn float_result(e: &mut Engine, sql: &str) -> f64 {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Float(x) => *x,
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        Value::Numeric { scaled, scale } => (*scaled as f64) / 10f64.powi(i32::from(*scale)),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn trunc_positive_fraction_truncates_down() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(1.7)"), 1.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(1.999)"), 1.0);
}

#[test]
fn trunc_negative_fraction_truncates_toward_zero() {
    // CRITICAL: distinct from floor.
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(-1.7)"), -1.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(-1.999)"), -1.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(-0.5)"), 0.0);
}

#[test]
fn trunc_half_step_truncates_not_rounds() {
    // trunc differs from round: trunc(1.5) → 1, NOT 2.
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(1.5)"), 1.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(2.5)"), 2.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(-1.5)"), -1.0);
}

#[test]
fn trunc_already_integer_passthrough() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(5.0)"), 5.0);
}

#[test]
fn trunc_zero() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(0.0)"), 0.0);
}

#[test]
fn trunc_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT trunc(42)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        Value::Float(x) => assert_eq!(*x, 42.0),
        Value::Numeric { scaled, scale } => {
            assert_eq!(*scaled, 42 * 10_i128.pow(u32::from(*scale)));
        }
        other => panic!("got {other:?}"),
    }
}

// ── TWO-ARG FORM ─────────────────────────────────────────────────

#[test]
fn trunc_scale_two_decimal_places() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(1.2345, 2)"), 1.23);
}

#[test]
fn trunc_scale_three_decimal_places() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(1.23456, 3)"), 1.234);
}

#[test]
fn trunc_scale_with_half_step() {
    // trunc(2.55, 1) → 2.5 (not rounded).
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(2.55, 1)"), 2.5);
}

#[test]
fn trunc_scale_negative_truncates_to_tens() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT trunc(15, -1)"), 10.0);
    assert_eq!(float_result(&mut e, "SELECT trunc(199, -2)"), 100.0);
}

#[test]
fn trunc_negative_value_with_scale() {
    let mut e = Engine::new();
    // trunc(-1.99, 1) → -1.9 (toward zero).
    assert_eq!(float_result(&mut e, "SELECT trunc(-1.99, 1)"), -1.9);
}

// ── NULL / ARITY ─────────────────────────────────────────────────

#[test]
fn trunc_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT trunc(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn trunc_null_scale_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT trunc(1.5, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn trunc_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT trunc()").is_err());
}

#[test]
fn trunc_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT trunc(1.5, 2, 3)").is_err());
}

#[test]
fn trunc_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT trunc('hello')").is_err());
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn trunc_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, x FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 1.9), (2, 2.9), (3, 3.9)")
        .unwrap();
    let r = e.execute("SELECT id FROM u WHERE trunc(x) = 2").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}
