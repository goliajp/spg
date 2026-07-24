//! read01 round 396 (MySQL differential) — `CHAR(n1, n2, …)` builds a
//! binary string from code points under the MySQL dialect.
//!
//! MySQL's `CHAR(n1, n2, …)` concatenates each integer's big-endian bytes
//! into a binary string (`CHAR(72, 73)` is 'HI', `CHAR(256)` is 0x0100), a
//! float argument rounds (`CHAR(66.5)` is 'C'), and a NULL argument is
//! skipped. SPG mapped it to nothing usable — a multi-arg call errored
//! ("function char(integer, integer) does not exist") and a single-arg one
//! returned a garbage `CHAR`-cast value. PostgreSQL has no `char()`
//! function, so the PG path is untouched.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// The rendered text of `expr` — CHAR returns a binary string that renders
/// latin1, so read it back through CONCAT to get the visible characters.
fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("`{sql}` not text: {other:?}"),
        },
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// Multiple code points concatenate into a string.
#[test]
fn char_multi_arg() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT CONCAT(CHAR(72, 73), '')"), "HI");
    assert_eq!(text(&mut e, "SELECT CONCAT(CHAR(72, 73, 33), '')"), "HI!");
    assert_eq!(text(&mut e, "SELECT CONCAT(CHAR(72), '')"), "H");
}

/// A float argument rounds; a NULL argument is skipped.
#[test]
fn rounding_and_null() {
    let mut e = mysql();
    // 66.5 -> 67 -> 'C'
    assert_eq!(text(&mut e, "SELECT CONCAT(CHAR(65, 66.5), '')"), "AC");
    assert_eq!(text(&mut e, "SELECT CONCAT(CHAR(65, NULL, 67), '')"), "AC");
}

/// A value over 255 encodes as multiple big-endian bytes.
#[test]
fn multibyte() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT HEX(CHAR(256))"), "0100");
    assert_eq!(text(&mut e, "SELECT HEX(CHAR(72))"), "48");
}

/// A PostgreSQL session has no `char()` function.
#[test]
fn postgres_no_char_function() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT CHAR(72, 73)").is_err(),
        "PG has no char() function"
    );
}
