//! read01 round 384 (MySQL differential) — a bitwise result stays an
//! integer type in an integer context.
//!
//! Round 383 made the MySQL bitwise operators return a scale-0 NUMERIC so
//! an unsigned value past i64::MAX (`~5`) has a home. But a bitwise result
//! most often feeds a function that wants an INTEGER — MAKE_SET's bit mask,
//! ELT / SUBSTRING / REPEAT counts — and those reject a NUMERIC, so
//! `MAKE_SET(1|4, …)`, `SUBSTRING(s, 1|2)` etc. (which worked before r383)
//! started to error. A bitwise result now stays a BigInt while it fits;
//! only a value past i64::MAX (bit 63 set) becomes a NUMERIC.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("`{sql}` not text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A bitwise mask feeds MAKE_SET (bits 1 and 4 -> members a and c).
#[test]
fn make_set_takes_a_bitwise_mask() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT MAKE_SET(1|4,'a','b','c','d')"), "a,c");
}

/// A bitwise count feeds SUBSTRING / REPEAT.
#[test]
fn substring_and_repeat_take_a_bitwise_count() {
    let mut e = mysql();
    // 1 | 2 = 3, SUBSTRING from position 3
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef',1|2)"), "cdef");
    // 1 | 1 = 1, REPEAT once
    assert_eq!(text(&mut e, "SELECT REPEAT('a',1|1)"), "a");
}

/// A result that fits i64 stays a BigInt (so downstream int consumers
/// keep taking it).
#[test]
fn result_type_narrows_when_it_fits() {
    let mut e = mysql();
    match e.execute("SELECT 5|2").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], Value::BigInt(7)),
        other => panic!("5|2: {other:?}"),
    }
}

/// A value with bit 63 set has no signed integer type, so it is a NUMERIC.
#[test]
fn overflow_value_is_numeric() {
    let mut e = mysql();
    match e.execute("SELECT 1<<63").unwrap() {
        QueryResult::Rows { rows, .. } => assert!(matches!(
            rows[0].values[0],
            Value::Numeric {
                scaled: 9_223_372_036_854_775_808,
                ..
            }
        )),
        other => panic!("1<<63: {other:?}"),
    }
    // One below the boundary still fits a signed BigInt.
    match e.execute("SELECT 1<<62").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::BigInt(4_611_686_018_427_387_904));
        }
        other => panic!("1<<62: {other:?}"),
    }
}
