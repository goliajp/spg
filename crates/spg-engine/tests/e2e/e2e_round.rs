//! PG `round(x)` / `round(x, scale)` — half-away-from-zero
//! rounding (NUMERIC semantic).
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-math.html
//!
//! Invariants pinned:
//!   * Half-away-from-zero rule (PG NUMERIC semantic):
//!     round(0.5) → 1; round(1.5) → 2; round(2.5) → 3;
//!     round(-0.5) → -1; round(-1.5) → -2.
//!   * Single-arg form rounds to nearest integer.
//!   * Two-arg form `round(x, n)` rounds to n decimal places:
//!     - n>0 → digits after decimal point
//!     - n<0 → round to nearest 10^|n|
//!     - n=0 → same as single-arg
//!   * Integer types passthrough unchanged.
//!   * NULL on any arg → NULL.
//!
//! Note: PG's Float (`double precision`) round() uses banker's
//! rounding (half to even), but the literal-without-cast form
//! (`round(2.5)`) is NUMERIC. SPG mirrors that. Apps casting
//! explicit floats to double would see different behavior;
//! that's documented as a known semantic edge case.

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
        other => panic!("expected numeric, got {other:?}"),
    }
}

// ── HALF-AWAY-FROM-ZERO RULE (CRITICAL) ──────────────────────────

#[test]
fn round_positive_half_step_rounds_up() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(0.5)"), 1.0);
    assert_eq!(float_result(&mut e, "SELECT round(1.5)"), 2.0);
    assert_eq!(float_result(&mut e, "SELECT round(2.5)"), 3.0);
    assert_eq!(float_result(&mut e, "SELECT round(3.5)"), 4.0);
}

#[test]
fn round_negative_half_step_rounds_down() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(-0.5)"), -1.0);
    assert_eq!(float_result(&mut e, "SELECT round(-1.5)"), -2.0);
    assert_eq!(float_result(&mut e, "SELECT round(-2.5)"), -3.0);
}

#[test]
fn round_quarter_step_nearest_wins() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(0.4)"), 0.0);
    assert_eq!(float_result(&mut e, "SELECT round(0.6)"), 1.0);
    assert_eq!(float_result(&mut e, "SELECT round(-0.4)"), 0.0);
    assert_eq!(float_result(&mut e, "SELECT round(-0.6)"), -1.0);
}

#[test]
fn round_zero() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(0.0)"), 0.0);
}

// ── TWO-ARG FORM (scale) ─────────────────────────────────────────

#[test]
fn round_scale_two_decimal_places() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(1.2345, 2)"), 1.23);
}

#[test]
fn round_scale_three_decimal_places() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(1.23456, 3)"), 1.235);
}

#[test]
fn round_scale_zero_same_as_single_arg() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(1.5, 0)"), 2.0);
}

#[test]
fn round_scale_negative_rounds_to_tens() {
    // PG: round(15, -1) → 20 (round to nearest 10).
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(15, -1)"), 20.0);
}

#[test]
fn round_scale_negative_two_rounds_to_hundreds() {
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(150, -2)"), 200.0);
}

#[test]
fn round_scale_with_half_step_preserves_rule() {
    // round(2.55, 1) → 2.6 (half-away-from-zero)
    let mut e = Engine::new();
    assert_eq!(float_result(&mut e, "SELECT round(2.55, 1)"), 2.6);
}

// ── INTEGER PASSTHROUGH ──────────────────────────────────────────

#[test]
fn round_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT round(42)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        Value::Float(x) => assert_eq!(*x, 42.0),
        Value::Numeric { scaled, scale, .. } => {
            assert_eq!(*scaled, 42 * 10_i128.pow(u32::from(*scale)));
        }
        other => panic!("got {other:?}"),
    }
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn round_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT round(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn round_null_scale_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT round(1.5, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn round_null_x_with_scale_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT round(NULL, 2)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY / TYPE ─────────────────────────────────────────────────

#[test]
fn round_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT round()").is_err());
}

#[test]
fn round_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT round(1.5, 2, 3)").is_err());
}

#[test]
fn round_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT round('hello')").is_err());
}

#[test]
fn round_non_integer_scale_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT round(1.5, 'foo')").is_err());
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn round_column_type_preserved() {
    let mut e = Engine::new();
    let r = e.execute("SELECT round(1.5)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    // PG returns same type as input (Numeric stays Numeric).
    assert!(matches!(
        columns[0].ty,
        spg_storage::DataType::Float | spg_storage::DataType::Numeric { .. }
    ));
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn round_currency_two_decimal_places() {
    // Realistic: round a price to cents.
    let mut e = Engine::new();
    e.execute("CREATE TABLE prices (id INT NOT NULL, p FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO prices VALUES (1, 9.997), (2, 14.501)")
        .unwrap();
    let r = e
        .execute("SELECT id, round(p, 2) FROM prices ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    match &rows[0].values[1] {
        Value::Float(x) => assert!((*x - 10.00).abs() < 1e-9, "got {x}"),
        other => panic!("got {other:?}"),
    }
    match &rows[1].values[1] {
        Value::Float(x) => assert!((*x - 14.50).abs() < 1e-9, "got {x}"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn round_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, x FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 2.4), (2, 2.6)")
        .unwrap();
    let r = e.execute("SELECT id FROM u WHERE round(x) = 3").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}
