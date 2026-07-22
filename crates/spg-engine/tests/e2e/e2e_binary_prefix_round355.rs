//! read01 round 355 (MySQL differential, M13) — the BINARY coercion.
//!
//! `BINARY 'a'` did not parse and `CAST(x AS BINARY)` was an unsupported
//! target. It is a COLLATION coercion, not a type change: MariaDB 11
//! renders `BINARY 'abc'` as `abc` and `HEX()` of it as `616263`, so the
//! value passes through unchanged. What it buys is byte-wise comparison
//! and LIKE — `'a' = 'A'` is 1 under MariaDB's default collation and
//! `BINARY 'a' = 'A'` is 0 — and `BINARY(n)` truncates to n bytes.
//!
//! SPG compares case-sensitively by default (the default-collation half
//! is M4 and still open), so the coercion's effect is观察-able today on a
//! column declared `COLLATE "case_insensitive"`, which is what the last
//! test here uses. Pinning only the default-collation cases would pass
//! for the wrong reason.

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

/// The value passes through — it is a collation, not a conversion.
#[test]
fn binary_keeps_the_value() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT BINARY 'abc'"), Value::text("abc"));
    assert_eq!(one(&mut e, "SELECT HEX(BINARY 'abc')"), Value::text("616263"));
    assert_eq!(one(&mut e, "SELECT LENGTH(BINARY 'héllo')"), Value::Int(6));
    assert_eq!(one(&mut e, "SELECT BINARY NULL"), Value::Null);
}

/// It binds tightly: MariaDB reads `BINARY 1 + 1` as `(BINARY 1) + 1`.
#[test]
fn it_binds_like_a_unary_operator() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT BINARY 1 + 1"), Value::BigInt(2));
}

/// `BINARY(n)` truncates to n bytes.
#[test]
fn a_length_truncates() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT CAST('abc' AS BINARY(2))"),
        Value::text("ab")
    );
    assert_eq!(
        one(&mut e, "SELECT CAST('abc' AS BINARY(9))"),
        Value::text("abc")
    );
}

/// The point of it: a comparison touching a BINARY operand is byte-wise
/// even when the other side is a case-insensitive column.
#[test]
fn it_refuses_case_folding() {
    let mut e = mysql();
    e.execute("CREATE TABLE ci (t TEXT COLLATE \"case_insensitive\")")
        .unwrap();
    e.execute("INSERT INTO ci VALUES ('Foo')").unwrap();

    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ci WHERE t = 'foo'"),
        Value::BigInt(1),
        "the column folds case on its own"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ci WHERE BINARY t = 'foo'"),
        Value::BigInt(0),
        "…and BINARY stops it"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ci WHERE t = BINARY 'foo'"),
        Value::BigInt(0),
        "from either side"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ci WHERE BINARY t = 'Foo'"),
        Value::BigInt(1),
        "an exact match still matches"
    );
}

/// PG has no such prefix, and says so.
#[test]
fn pg_has_no_binary_prefix() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT BINARY 'abc'").is_err());
}
