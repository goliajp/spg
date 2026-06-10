//! PG `mod(y, x)` — modulo. Result's sign follows the dividend `y`.
//!
//! Note PG also accepts `y % x` infix; this test surface targets
//! the function form which is what apps emit when they want
//! NUMERIC support (% has integer-only specialisations).
//!
//! Invariants pinned:
//!   * sign-of-result follows dividend (NOT divisor).
//!     mod(7, 3)   = 1
//!     mod(-7, 3)  = -1
//!     mod(7, -3)  = 1
//!     mod(-7, -3) = -1
//!   * mod(_, 0) → error (division-by-zero).
//!   * NULL on any arg → NULL.

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

fn int_result(e: &mut Engine, sql: &str) -> i64 {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Int(n) => i64::from(*n),
        Value::BigInt(n) => *n,
        Value::SmallInt(n) => i64::from(*n),
        other => panic!("expected int, got {other:?}"),
    }
}

#[test]
fn mod_basic_positive() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(7, 3)"), 1);
}

#[test]
fn mod_exact_multiple_zero() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(10, 5)"), 0);
}

#[test]
fn mod_dividend_smaller_than_divisor() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(3, 7)"), 3);
}

#[test]
fn mod_zero_dividend() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(0, 5)"), 0);
}

// ── SIGN FOLLOWS DIVIDEND (CRITICAL) ─────────────────────────────

#[test]
fn mod_negative_dividend_yields_negative_result() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(-7, 3)"), -1);
}

#[test]
fn mod_negative_divisor_yields_positive_result() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(7, -3)"), 1);
}

#[test]
fn mod_both_negative_yields_negative_result() {
    let mut e = Engine::new();
    assert_eq!(int_result(&mut e, "SELECT mod(-7, -3)"), -1);
}

// ── DIVISION BY ZERO ─────────────────────────────────────────────

#[test]
fn mod_by_zero_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT mod(7, 0)").is_err());
}

#[test]
fn mod_zero_by_zero_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT mod(0, 0)").is_err());
}

// ── NULL HANDLING ────────────────────────────────────────────────

#[test]
fn mod_null_dividend_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT mod(NULL, 3)").unwrap());
    assert_eq!(row[0], Value::Null);
}

#[test]
fn mod_null_divisor_returns_null() {
    let mut e = Engine::new();
    let row = one_row(e.execute("SELECT mod(7, NULL)").unwrap());
    assert_eq!(row[0], Value::Null);
}

// ── ARITY ────────────────────────────────────────────────────────

#[test]
fn mod_one_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT mod(7)").is_err());
}

#[test]
fn mod_three_args_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT mod(7, 3, 1)").is_err());
}

#[test]
fn mod_text_arg_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT mod('a', 3)").is_err());
}

// ── LARGE NUMBERS ────────────────────────────────────────────────

#[test]
fn mod_bigint() {
    let mut e = Engine::new();
    // 1000000007 = 142857143 * 7 + 6.
    assert_eq!(int_result(&mut e, "SELECT mod(1000000007, 7)"), 6);
}

// ── INTEGRATION ─────────────────────────────────────────────────

#[test]
fn mod_inside_where_for_even_filter() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (n INT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (1), (2), (3), (4), (5)")
        .unwrap();
    let r = e
        .execute("SELECT n FROM u WHERE mod(n, 2) = 0 ORDER BY n")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(2));
    assert_eq!(rows[1].values[0], Value::Int(4));
}

// ── COLUMN TYPE ──────────────────────────────────────────────────

#[test]
fn mod_column_type_is_int_for_int_inputs() {
    let mut e = Engine::new();
    let r = e.execute("SELECT mod(7, 3)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert!(matches!(
        columns[0].ty,
        spg_storage::DataType::Int
            | spg_storage::DataType::BigInt
            | spg_storage::DataType::SmallInt
    ));
}
