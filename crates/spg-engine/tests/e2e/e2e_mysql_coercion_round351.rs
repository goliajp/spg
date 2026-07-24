//! read01 round 351 (MySQL differential, M11) — a string in numeric position.
//!
//! MySQL reads a string as its LEADING number wherever a number is wanted:
//! `'1abc'+0` is 1, `'abc'+0` is 0, `'2024-01-15'+0` is 2024. SPG already
//! coerced a CLEANLY numeric string — that is PG's unknown-literal
//! resolution and it stays — so everything else was
//! `operator does not exist: text + integer`, including plain `'1.5'+1`.
//!
//! Two things surfaced while measuring, both about `/`:
//!   * MySQL's `/` is NEVER integer division — MariaDB answers 2.5000 for
//!     `10/4` — while SPG truncated to 2, so a division of two integers
//!     came back short with nothing to show for it. `DIV` is the spelling
//!     that truncates. PG's `/` IS integer division and is untouched.
//!   * `10/0` answers NULL in MariaDB rather than raising.
//!
//! Every expectation below is copied from the MariaDB 11 run.

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

/// The leading-number rule, value by value.
#[test]
fn a_string_contributes_its_leading_number() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT '1abc'+0", Value::BigInt(1)),
        ("SELECT 'abc'+0", Value::BigInt(0)),
        ("SELECT '2024-01-15'+0", Value::BigInt(2024)),
        ("SELECT ''+0", Value::BigInt(0)),
        ("SELECT ' 12 '+0", Value::BigInt(12)),
        ("SELECT '-5'+0", Value::BigInt(-5)),
        ("SELECT '1.5'+1", Value::Float(2.5)),
        ("SELECT '.5'+0", Value::Float(0.5)),
        // The exponent form counts.
        ("SELECT '1e3'+0", Value::BigInt(1000)),
        ("SELECT '1E3'+0", Value::BigInt(1000)),
        ("SELECT '1.5e2'+0", Value::BigInt(150)),
        ("SELECT '-1e2'+0", Value::BigInt(-100)),
        // …but a trailing `e` with no digits is not part of it.
        ("SELECT '1e'+0", Value::BigInt(1)),
    ] {
        assert_eq!(one(&mut e, sql), want, "for `{sql}`");
    }
}

/// Unary minus and string-only arithmetic.
#[test]
fn unary_minus_and_two_strings() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT -'5'"), Value::BigInt(-5));
    assert_eq!(one(&mut e, "SELECT -'abc'"), Value::BigInt(0));
    assert_eq!(one(&mut e, "SELECT -'1.5'"), Value::Float(-1.5));
    assert_eq!(one(&mut e, "SELECT '10'*'2'"), Value::BigInt(20));
    assert_eq!(one(&mut e, "SELECT '10'/'4'"), Value::Float(2.5));
}

/// A MIXED pair compares numerically; two strings compare as strings.
/// That difference is measured, and it is easy to get wrong.
#[test]
fn comparison_converts_only_a_mixed_pair() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT '10' > 9"), Value::Bool(true));
    assert_eq!(
        one(&mut e, "SELECT '10' > '9'"),
        Value::Bool(false),
        "two strings compare as strings — MariaDB answers 0 here"
    );
    assert_eq!(one(&mut e, "SELECT 'abc' = 0"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT '1' = 1"), Value::Bool(true));
}

/// `/` is a real division in MySQL and integer division in PG.
#[test]
fn division_splits_by_dialect() {
    let mut m = mysql();
    assert_eq!(one(&mut m, "SELECT 10/4"), Value::numeric(25000, 4));
    assert_eq!(one(&mut m, "SELECT 7/2"), Value::numeric(35000, 4));
    assert_eq!(one(&mut m, "SELECT -7/2"), Value::numeric(-35000, 4));
    assert_eq!(one(&mut m, "SELECT 10/0"), Value::Null, "MariaDB: NULL");

    let mut p = Engine::new();
    assert_eq!(one(&mut p, "SELECT 10/4"), Value::Int(2), "PG truncates");
    assert_eq!(one(&mut p, "SELECT 7/2"), Value::Int(3));
    assert!(p.execute("SELECT 10/0").is_err(), "PG raises");
}

/// The PG dialect keeps PG's refusal — a bare literal that is not a
/// number is still an error there.
#[test]
fn pg_still_refuses_a_non_numeric_string() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT '1abc'+0").is_err());
    assert!(p.execute("SELECT 'abc' = 0").is_err());
    // …while the cleanly numeric one resolves, as PG resolves `unknown`.
    assert_eq!(one(&mut p, "SELECT '10' + 5"), Value::Int(15));
}
