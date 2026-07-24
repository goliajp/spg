//! read01 round 395 (MySQL differential) — LEFT / RIGHT with a negative
//! length is the empty string under the MySQL dialect.
//!
//! MariaDB `LEFT('abc', -1)` and `RIGHT('abc', -1)` are the empty string (a
//! non-positive length yields nothing). PostgreSQL reads a negative length
//! as "all but the last / first |k| characters" (`left('abc', -1)` is
//! 'ab'), which is what SPG did — a silent-wrong under the MySQL dialect
//! (`LEFT('abcdef', -1)` returned 'abcde' instead of ''). A positive /
//! zero / oversized length is unchanged, and a PostgreSQL session keeps the
//! drop-from-end reading.
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

/// A negative length is the empty string.
#[test]
fn negative_length_is_empty() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', -1)"), "");
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', -100)"), "");
    assert_eq!(text(&mut e, "SELECT RIGHT('abcdef', -1)"), "");
    assert_eq!(text(&mut e, "SELECT RIGHT('abcdef', -100)"), "");
}

/// Positive / zero / oversized lengths are unchanged.
#[test]
fn non_negative_unchanged() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', 3)"), "abc");
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', 0)"), "");
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', 100)"), "abcdef");
    assert_eq!(text(&mut e, "SELECT RIGHT('abcdef', 2)"), "ef");
}

/// A PostgreSQL session keeps the drop-last / drop-first reading.
#[test]
fn postgres_drops_from_end() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT LEFT('abcdef', -1)"), "abcde");
    assert_eq!(text(&mut e, "SELECT RIGHT('abcdef', -1)"), "bcdef");
}
