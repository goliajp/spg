//! PG `floor(x)` — largest integer <= x.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-math.html
//!
//! Invariants pinned:
//!   * Integer types passthrough unchanged.
//!   * Float type → Float (PG returns double precision when
//!     input is double, NUMERIC when input is NUMERIC). SPG
//!     keeps Float→Float.
//!   * Negative floats floor TOWARD -infinity, NOT toward zero.
//!     floor(-1.5) → -2, NOT -1.
//!   * NULL → NULL.
//!   * floor(0) → 0; floor(-0.0) → 0.

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

// ── BASIC ────────────────────────────────────────────────────────

#[test]
fn floor_positive_fraction() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(1.7)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 1.0),
        Value::Numeric { scaled, scale } => {
            assert_eq!(*scaled, 10_i128.pow(u32::from(*scale)));
        }
        other => panic!("expected numeric, got {other:?}"),
    }
}

#[test]
fn floor_negative_fraction_rounds_toward_neg_infinity() {
    // CRITICAL: floor(-1.5) → -2 (PG-canonical), NOT -1.
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(-1.5)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, -2.0),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn floor_negative_half_step() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(-0.5)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, -1.0),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn floor_already_integer_float() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(5.0)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 5.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_zero() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(0.0)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 0.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(42)").unwrap());
    // PG returns an integer in this case but SPG can return
    // BigInt or matching int type; assert it's the right value
    // regardless of width.
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, 42),
        Value::BigInt(n) => assert_eq!(*n, 42),
        Value::Float(x) => assert_eq!(*x, 42.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_negative_integer_passthrough() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(-7)").unwrap());
    match &row[0] {
        Value::Int(n) => assert_eq!(*n, -7),
        Value::BigInt(n) => assert_eq!(*n, -7),
        Value::Float(x) => assert_eq!(*x, -7.0),
        other => panic!("got {other:?}"),
    }
}

// ── EDGE CASES ───────────────────────────────────────────────────

#[test]
fn floor_just_below_integer_negative() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(-2.999)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, -3.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_just_above_integer_negative() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(-2.001)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, -3.0),
        other => panic!("got {other:?}"),
    }
}

// ── NULL ─────────────────────────────────────────────────────────

#[test]
fn floor_null_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT floor(NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn floor_zero_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT floor()").is_err());
}

#[test]
fn floor_too_many_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT floor(1.5, 2)").is_err());
}

#[test]
fn floor_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT floor('hello')").is_err());
}

// ── COLUMN TYPE (sqlx decoder) ───────────────────────────────────

#[test]
fn floor_float_column_type_is_float() {
    let mut e = Engine::new();
    let r = e.execute("SELECT floor(1.5)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    // PG returns same type as input — Float in / Float out.
    assert_eq!(columns[0].ty, spg_storage::DataType::Float);
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn floor_inside_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, x FLOAT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 1.5), (2, 2.5), (3, 3.5)")
        .unwrap();
    let r = e.execute("SELECT id FROM u WHERE floor(x) = 2").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(2));
}

#[test]
fn floor_inside_insert_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n FLOAT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (floor(3.9))").unwrap();
    let row = one_row(e.execute("SELECT n FROM u").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 3.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_arithmetic_composition() {
    let mut e = Engine::new();
    // floor(x/y) for integer-division simulation.
    let row = one_row(e.execute("SELECT floor(10.0 / 3.0)").unwrap());
    match &row[0] {
        Value::Float(x) => assert_eq!(*x, 3.0),
        other => panic!("got {other:?}"),
    }
}
