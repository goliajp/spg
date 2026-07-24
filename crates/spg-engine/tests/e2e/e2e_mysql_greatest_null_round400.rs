//! read01 round 400 (MySQL differential) — GREATEST / LEAST return NULL
//! when any argument is NULL, under the MySQL dialect.
//!
//! MySQL `GREATEST(1, NULL, 3)` and `LEAST(5, NULL, 2)` are NULL — any NULL
//! argument poisons the result. PostgreSQL ignores NULLs and returns the
//! greatest / least non-null value (`GREATEST(1, NULL, 3)` is 3), which is
//! what SPG did, so a MySQL query got a non-NULL where it expected NULL — a
//! silent-wrong. A call with no NULL is unchanged, and a PostgreSQL session
//! keeps the ignore-NULL behavior.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// Any NULL argument makes the result NULL.
#[test]
fn null_arg_poisons() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT GREATEST(1, NULL, 3)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT LEAST(5, NULL, 2)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT GREATEST(NULL)"), Value::Null);
    assert_eq!(one(&mut e, "SELECT LEAST('a', NULL)"), Value::Null);
}

/// A call with no NULL is unchanged.
#[test]
fn no_null_unchanged() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT GREATEST(1, 2, 3)"), Value::Int(3));
    assert_eq!(one(&mut e, "SELECT LEAST(5, 1, 2)"), Value::Int(1));
}

/// A PostgreSQL session ignores NULLs (returns the greatest / least
/// non-null).
#[test]
fn postgres_ignores_null() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT GREATEST(1, NULL, 3)"), Value::Int(3));
    assert_eq!(one(&mut e, "SELECT LEAST(5, NULL, 2)"), Value::Int(2));
}
