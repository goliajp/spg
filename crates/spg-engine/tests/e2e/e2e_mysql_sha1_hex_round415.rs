//! read01 round 415 (MySQL differential) — SHA1 returns hex text under the
//! MySQL dialect.
//!
//! MariaDB's `SHA1()` (and its `SHA()` alias) returns the 40-character
//! lowercase hex digest as TEXT, so `SHA1(x) = 'abc…'` compares directly
//! against a stored hash. SPG followed the PG spec and returned BYTEA — the
//! `\x…` shape — which byte-compared unequal to any hex string a caller
//! stored, silently breaking hash comparisons and dashboards. PG's `sha1()`
//! keeps its bytea return.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

/// SHA1 under the MySQL dialect is 40-char lowercase hex text.
#[test]
fn mysql_sha1_is_hex_text() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT SHA1('abc')"),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        scalar(&mut e, "SELECT SHA1('')"),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    // The text form has length 40 (not the 20 bytes the digest is).
    assert_eq!(scalar(&mut e, "SELECT LENGTH(SHA1('abc'))"), "40");
}

/// A caller can compare `SHA1(x) = '…'` directly against a stored hex
/// string — the whole point of returning text.
#[test]
fn mysql_sha1_compares_to_hex_literal() {
    let mut e = mysql();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT SHA1('abc') = 'a9993e364706816aba3e25717850c26c9cd0d89d'"
        ),
        "true"
    );
}

/// `SHA1` and `SHA` are aliases (both return the same hex text under MySQL).
#[test]
fn mysql_sha_alias_matches_sha1() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT SHA1('abc') = SHA('abc')"), "true");
}

/// NULL propagates.
#[test]
fn mysql_sha1_null() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT SHA1(NULL)"), "NULL");
}

/// A PostgreSQL session keeps `sha1()` returning BYTEA (the pgcrypto shape;
/// 20 bytes long, renders `\x…`).
#[test]
fn postgres_sha1_stays_bytea() {
    let mut e = Engine::new();
    // 20 bytes in the digest → PG octet_length of the bytea is 20.
    assert_eq!(scalar(&mut e, "SELECT octet_length(sha1('abc'))"), "20");
    // The value renders as `\x…`, not raw hex text.
    let s = scalar(&mut e, "SELECT sha1('abc')");
    assert!(s.starts_with("\\x"), "PG sha1 should render bytea `\\x…`, got {s:?}");
}
