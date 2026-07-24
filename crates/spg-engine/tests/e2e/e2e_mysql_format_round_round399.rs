//! read01 round 399 (MySQL differential) — FORMAT() rounds half away from
//! zero, not to even.
//!
//! MariaDB `FORMAT(n, d)` rounds the half case AWAY from zero:
//! `FORMAT(2.5, 0)` is 3, `FORMAT(0.5, 0)` is 1, `FORMAT(1234.5, 0)` is
//! 1,235. SPG rendered through Rust's `{:.d$}`, which rounds half to EVEN
//! (2, 0, 1,234), so any `.5` at the rounding digit landed on the wrong
//! side — a silent-wrong. The grouped thousands separators and the decimal
//! places are unchanged.
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
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// A half case rounds away from zero.
#[test]
fn half_rounds_away() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT FORMAT(2.5, 0)"), "3");
    assert_eq!(text(&mut e, "SELECT FORMAT(0.5, 0)"), "1");
    assert_eq!(text(&mut e, "SELECT FORMAT(1.5, 0)"), "2");
    assert_eq!(text(&mut e, "SELECT FORMAT(1234.5, 0)"), "1,235");
    assert_eq!(text(&mut e, "SELECT FORMAT(-1234.5, 0)"), "-1,235");
}

/// A non-half fraction rounds normally.
#[test]
fn non_half_unchanged() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT FORMAT(1233.5, 0)"), "1,234");
    assert_eq!(text(&mut e, "SELECT FORMAT(3.14159, 2)"), "3.14");
    assert_eq!(text(&mut e, "SELECT FORMAT(1234567.891, 2)"), "1,234,567.89");
}

/// Carry propagation and the decimal places / grouping.
#[test]
fn carry_and_layout() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT FORMAT(999.999, 2)"), "1,000.00");
    assert_eq!(text(&mut e, "SELECT FORMAT(1234.5, 1)"), "1,234.5");
    assert_eq!(text(&mut e, "SELECT FORMAT(0, 2)"), "0.00");
}
