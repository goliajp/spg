//! read01 round 375 (MySQL differential) — the MySQL default collation
//! is PAD SPACE: trailing spaces are ignored when text is COMPARED,
//! GROUPED, de-duped or sorted (but NOT in LIKE, and NOT under BINARY).
//!
//! MariaDB 11: `'a' = 'a '` and `'' = ' '` are both 1, `'a' < 'a '` is 0
//! (they compare equal), and a UNIQUE / GROUP BY / DISTINCT collapses
//! `'a'` with `'a '`. SPG compared byte-wise, so `WHERE t = 'a'` missed
//! the space-padded rows and `COUNT(DISTINCT t)` counted them separately.
//! `LIKE` treats a trailing space literally (`'a ' LIKE 'a'` is 0), and
//! `BINARY` / a `utf8mb4_bin` column force a byte-wise compare — both
//! keep the space significant. Storage is untouched: `LENGTH('a ')` is
//! still 2.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
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

fn count(e: &mut Engine, sql: &str) -> i64 {
    match scalar(e, sql) {
        Value::BigInt(n) => n,
        other => panic!("`{sql}` not a count: {other:?}"),
    }
}

/// Trailing spaces are ignored in a comparison.
#[test]
fn comparison_ignores_trailing_spaces() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a '"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a  '"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT '' = ' '"), Value::Bool(true));
    // …but a padded value still orders equal, not less.
    assert_eq!(scalar(&mut e, "SELECT 'a' < 'a '"), Value::Bool(false));
    // A non-trailing space is significant.
    assert_eq!(scalar(&mut e, "SELECT 'a' < 'a b'"), Value::Bool(true));
    // Only spaces pad — a tab is significant.
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'a\t'"), Value::Bool(false));
}

/// WHERE / DISTINCT / GROUP BY collapse space-padded variants.
#[test]
fn where_distinct_group_collapse_padding() {
    let mut e = mysql();
    e.execute("CREATE TABLE s (t VARCHAR(10))").unwrap();
    e.execute("INSERT INTO s VALUES ('a'),('a '),('a  '),('b')")
        .unwrap();
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM s WHERE t = 'a'"), 3);
    assert_eq!(count(&mut e, "SELECT COUNT(DISTINCT t) FROM s"), 2);
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM (SELECT t FROM s GROUP BY t) g"),
        2
    );
}

/// A UNIQUE constraint treats `'a'` and `'a '` as the same key.
#[test]
fn unique_collapses_padding() {
    let mut e = mysql();
    e.execute("CREATE TABLE u (t VARCHAR(10) UNIQUE)").unwrap();
    e.execute("INSERT INTO u VALUES ('a')").unwrap();
    assert!(e.execute("INSERT INTO u VALUES ('a ')").is_err());
}

/// LIKE and BINARY keep the trailing space significant.
#[test]
fn like_and_binary_keep_the_space() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'a ' LIKE 'a'"), Value::Bool(false));
    assert_eq!(scalar(&mut e, "SELECT 'a' LIKE 'a '"), Value::Bool(false));
    assert_eq!(
        scalar(&mut e, "SELECT 'a' = BINARY 'a '"),
        Value::Bool(false)
    );
    // Storage keeps the space — only comparison ignores it.
    assert_eq!(scalar(&mut e, "SELECT LENGTH('a ')"), Value::Int(2));
}

/// A PostgreSQL session compares byte-wise — trailing spaces matter.
#[test]
fn postgres_session_keeps_trailing_spaces() {
    let mut p = Engine::new();
    assert_eq!(scalar(&mut p, "SELECT 'a' = 'a '"), Value::Bool(false));
}
