//! read01 round 372 (MySQL differential) — `x % 0` and `MOD(x, 0)` are
//! NULL under the MySQL dialect, not the PostgreSQL "division by zero"
//! error.
//!
//! MariaDB 11 returns NULL for every zero-divisor form — `1/0`, `1 DIV
//! 0`, `10 % 0`, `MOD(10, 0)`, `10.5 % 0` — with a warning, never an
//! error. SPG already matched `/` and `DIV`, but `%` and `MOD()` still
//! raised PG's 22012 error, so a MySQL query that hit a zero divisor in a
//! remainder failed outright instead of yielding NULL. A PostgreSQL
//! session keeps the honest error.
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

/// Every zero-divisor remainder is NULL under the dialect.
#[test]
fn modulo_by_zero_is_null() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 10 % 0"), Value::Null);
    assert_eq!(scalar(&mut e, "SELECT MOD(10, 0)"), Value::Null);
    assert_eq!(scalar(&mut e, "SELECT 10.5 % 0"), Value::Null);
    assert_eq!(scalar(&mut e, "SELECT MOD(10.5, 0)"), Value::Null);
    // …consistent with division, which already returned NULL.
    assert_eq!(scalar(&mut e, "SELECT 1 / 0"), Value::Null);
}

/// A non-zero divisor is unaffected.
#[test]
fn nonzero_modulo_still_computes() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 10 % 3"), Value::Int(1));
    assert_eq!(scalar(&mut e, "SELECT MOD(10, 3)"), Value::Int(1));
}

/// A PostgreSQL session raises the honest division-by-zero error.
#[test]
fn postgres_session_errors_on_modulo_by_zero() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT 10 % 0").is_err());
    assert!(p.execute("SELECT MOD(10, 0)").is_err());
}
