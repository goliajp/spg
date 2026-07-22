//! read01 round 352 (MySQL differential, M8) — CAST AS SIGNED / UNSIGNED.
//!
//! `CAST(x AS SIGNED)` is how MySQL code turns anything into an integer,
//! and it was `unsupported cast target ::signed`. PG has no such type —
//! `type "signed" does not exist`, measured — so the reading is gated on
//! the dialect and not merely on the spelling.
//!
//! MariaDB 11, measured: a string gives its LEADING number (`'12abc'` is
//! 12, `'abc'` is 0); a fractional value ROUNDS half-away-from-zero
//! (1.5 → 2, 2.5 → 3, -2.5 → -3) rather than truncating; UNSIGNED wraps a
//! negative through u64 (`-1` → 18446744073709551615); and the optional
//! `INTEGER` / `INT` tail is accepted.
//!
//! One silent-wrong came with it: `CAST(123 AS CHAR)` answered **'1'**.
//! MySQL's CHAR-without-length is UNBOUNDED and MariaDB answers '123';
//! the SQL-standard reading (PG's) is char(1). Truncating a number to its
//! first digit, with no error, on a MySQL session.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A string gives its leading number, exactly as in arithmetic.
#[test]
fn a_string_casts_to_its_leading_number() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT CAST('12abc' AS SIGNED)"), Value::BigInt(12));
    assert_eq!(one(&mut e, "SELECT CAST('abc' AS SIGNED)"), Value::BigInt(0));
    assert_eq!(one(&mut e, "SELECT CAST('12' AS UNSIGNED)"), Value::BigInt(12));
}

/// Half away from zero — NOT truncation, which is the easy thing to
/// assume and the wrong one.
#[test]
fn a_fraction_rounds_half_away_from_zero() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT CAST(1.5 AS SIGNED)", 2),
        ("SELECT CAST(2.5 AS SIGNED)", 3),
        ("SELECT CAST(-1.5 AS SIGNED)", -2),
        ("SELECT CAST(-2.5 AS SIGNED)", -3),
        ("SELECT CAST(1.4 AS SIGNED)", 1),
    ] {
        assert_eq!(one(&mut e, sql), Value::BigInt(want), "for `{sql}`");
    }
}

/// UNSIGNED wraps a negative through the full u64 range.
#[test]
fn unsigned_wraps_a_negative() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT CAST(-1 AS UNSIGNED)"),
        Value::numeric(18_446_744_073_709_551_615, 0),
    );
}

/// The `INTEGER` / `INT` tail MariaDB also accepts.
#[test]
fn the_integer_tail_parses() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT CAST(1 AS SIGNED INTEGER)"), Value::BigInt(1));
    assert_eq!(one(&mut e, "SELECT CAST('9' AS SIGNED INT)"), Value::BigInt(9));
    assert_eq!(
        one(&mut e, "SELECT CAST(7 AS UNSIGNED INTEGER)"),
        Value::BigInt(7)
    );
}

/// `CAST(x AS CHAR)` keeps the whole value in MySQL and one character in
/// PG. It used to keep one character in both.
#[test]
fn char_without_a_length_splits_by_dialect() {
    let mut m = mysql();
    assert_eq!(one(&mut m, "SELECT CAST(123 AS CHAR)"), Value::text("123"));
    assert_eq!(
        one(&mut m, "SELECT CONCAT(CAST(1 AS CHAR), 'x')"),
        Value::text("1x")
    );
    // An explicit length still bounds it.
    assert_eq!(one(&mut m, "SELECT CAST(123 AS CHAR(2))"), Value::BpChar("12".into()));

    let mut p = Engine::new();
    assert_eq!(
        one(&mut p, "SELECT CAST(123 AS CHAR)"),
        Value::BpChar("1".into()),
        "the SQL-standard reading is char(1)"
    );
}

/// PG has no SIGNED / UNSIGNED at all, and still says so.
#[test]
fn pg_has_no_such_type() {
    let mut p = Engine::new();
    for sql in [
        "SELECT CAST('12' AS SIGNED)",
        "SELECT CAST(1 AS UNSIGNED)",
    ] {
        assert!(p.execute(sql).is_err(), "PG: type \"signed\" does not exist");
    }
}
