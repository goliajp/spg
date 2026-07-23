//! read01 round 376 (MySQL differential) — SUBSTRING / SUBSTR / MID
//! index a negative start from the END on the MySQL dialect, and return
//! the empty string for start 0, an out-of-range negative start, or a
//! negative length.
//!
//! MariaDB 11: `SUBSTRING('abcdef', -2)` is 'ef', `SUBSTRING('abcdef', 0)`
//! and `SUBSTRING('abcdef', -10)` are '', and a negative length is ''.
//! SPG used PG's rule — a negative or zero start clamps to 1, so
//! `SUBSTRING('abcdef', -2)` returned the whole string and a negative
//! length raised — silently wrong for any MySQL query that extracts a
//! suffix with a negative index. A PostgreSQL session keeps PG's rule.
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
        QueryResult::Rows { rows, .. } => match rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
        {
            Some(Value::Text(s)) => s.into_owned(),
            other => panic!("`{sql}` not text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A negative start counts from the end.
#[test]
fn negative_start_counts_from_the_end() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', -2)"), "ef");
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', -2, 1)"), "e");
    assert_eq!(text(&mut e, "SELECT SUBSTR('abcdef', -3, 2)"), "de");
    assert_eq!(text(&mut e, "SELECT MID('abcdef', -2, 2)"), "ef");
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef' FROM -2)"), "ef");
}

/// Start 0, an out-of-range negative start, and a negative length are all
/// the empty string.
#[test]
fn out_of_range_is_empty() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', 0)"), "");
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', -10)"), "");
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', 3, -1)"), "");
}

/// Positive-start forms are unchanged.
#[test]
fn positive_start_unchanged() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', 2)"), "bcdef");
    assert_eq!(text(&mut e, "SELECT SUBSTRING('abcdef', 2, 3)"), "bcd");
}

/// A PostgreSQL session keeps PG's rule: a negative start clamps to 1, so
/// the whole string comes back.
#[test]
fn postgres_session_clamps_to_one() {
    let mut p = Engine::new();
    assert_eq!(text(&mut p, "SELECT SUBSTRING('abcdef', -2)"), "abcdef");
}
