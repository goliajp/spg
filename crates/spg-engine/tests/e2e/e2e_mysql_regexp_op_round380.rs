//! read01 round 380 (MySQL differential) — the REGEXP / RLIKE regex-match
//! operator (and its NOT form).
//!
//! MariaDB matches a POSIX regex with the infix `s REGEXP pat` operator
//! (RLIKE is the alias), case-insensitively under the default collation:
//! `'abc' REGEXP 'b'` and `'ABC' REGEXP 'abc'` are both 1. SPG had the
//! `REGEXP_*` functions and the PG `~` operator, but not the MySQL
//! keyword operator, so every `WHERE col REGEXP ...` / `col RLIKE ...`
//! query failed with a syntax error. It now lowers onto the same
//! `regexp_like(expr, pat, 'i')` the `~*` operator uses, with `NOT`
//! negating it. PG has no REGEXP keyword and is unaffected.
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

/// REGEXP and its RLIKE alias match a POSIX pattern, case-insensitively.
#[test]
fn regexp_and_rlike_match() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'abc' REGEXP 'b'"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT 'abc' RLIKE '^a'"), Value::Bool(true));
    assert_eq!(
        scalar(&mut e, "SELECT 'abc' REGEXP 'z'"),
        Value::Bool(false)
    );
    // Case-insensitive under the default collation.
    assert_eq!(
        scalar(&mut e, "SELECT 'ABC' REGEXP 'abc'"),
        Value::Bool(true)
    );
}

/// NOT REGEXP / NOT RLIKE negate the match.
#[test]
fn not_regexp_negates() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT 'abc' NOT REGEXP 'z'"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut e, "SELECT 'abc' NOT RLIKE 'b'"),
        Value::Bool(false)
    );
}

/// The operator works in WHERE.
#[test]
fn regexp_in_where() {
    let mut e = mysql();
    e.execute("CREATE TABLE t (s VARCHAR(20))").unwrap();
    e.execute("INSERT INTO t VALUES ('apple'), ('banana'), ('cherry')")
        .unwrap();
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM t WHERE s REGEXP 'an'"),
        1
    );
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM t WHERE s REGEXP 'e'"),
        2
    );
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM t WHERE s NOT RLIKE 'a'"),
        1
    );
}

/// A PostgreSQL session has no REGEXP keyword operator.
#[test]
fn postgres_session_has_no_regexp_keyword() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT 'abc' REGEXP 'b'").is_err());
}
