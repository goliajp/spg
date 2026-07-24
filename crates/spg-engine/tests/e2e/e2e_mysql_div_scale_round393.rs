//! read01 round 393 (MySQL differential) — `/` on exact operands is a
//! DECIMAL whose scale is the LEFT operand's scale + 4.
//!
//! MariaDB's `/` on integer / decimal operands produces a DECIMAL, not a
//! float: `7/2` is 3.5000, `10.0/3` is 3.33333, `7.00/2` is 3.500000 — the
//! result scale is the dividend's scale plus `div_precision_increment` (4).
//! SPG returned a float (`3.5`) for integer division and a scale-16 NUMERIC
//! for decimal division, so the rendered precision diverged. A zero divisor
//! is NULL; a float / double operand keeps the float result; a PostgreSQL
//! session keeps PG's `/` semantics (integer division truncates).
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// `(scaled, scale)` of a NUMERIC result.
fn dec(e: &mut Engine, sql: &str) -> (i128, u16) {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Numeric { scaled, scale, .. } => (*scaled, *scale),
            other => panic!("`{sql}` not numeric: {other:?}"),
        },
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// Integer division is a DECIMAL scaled to 4 places.
#[test]
fn integer_division_scale_4() {
    let mut e = mysql();
    assert_eq!(dec(&mut e, "SELECT 7/2"), (35000, 4));
    assert_eq!(dec(&mut e, "SELECT 100/7"), (142857, 4));
    assert_eq!(dec(&mut e, "SELECT 1/3"), (3333, 4));
    assert_eq!(dec(&mut e, "SELECT -7/2"), (-35000, 4));
}

/// The scale follows the LEFT operand's scale + 4.
#[test]
fn scale_follows_dividend() {
    let mut e = mysql();
    assert_eq!(dec(&mut e, "SELECT 7.00/2"), (3_500_000, 6));
    assert_eq!(dec(&mut e, "SELECT 7/2.0"), (35000, 4)); // right scale ignored
    assert_eq!(dec(&mut e, "SELECT 10.0/3"), (333_333, 5));
    assert_eq!(dec(&mut e, "SELECT 1.5/0.3"), (500_000, 5));
}

/// A zero divisor is NULL.
#[test]
fn divide_by_zero_is_null() {
    let mut e = mysql();
    assert!(matches!(
        e.execute("SELECT 1/0").unwrap(),
        QueryResult::Rows { rows, .. } if rows[0].values[0] == Value::Null
    ));
}

/// A float operand keeps a float result.
#[test]
fn float_operand_stays_float() {
    let mut e = mysql();
    match e.execute("SELECT 7.5/CAST(2 AS DOUBLE)") {
        Ok(QueryResult::Rows { rows, .. }) => {
            assert!(matches!(rows[0].values[0], Value::Float(_)));
        }
        other => panic!("{other:?}"),
    }
}

/// A PostgreSQL session keeps PG's `/` (integer division truncates to 3).
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    match e.execute("SELECT 7/2").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], Value::Int(3)),
        other => panic!("{other:?}"),
    }
}
