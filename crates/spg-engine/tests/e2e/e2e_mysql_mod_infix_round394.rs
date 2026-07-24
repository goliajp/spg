//! read01 round 394 (MySQL differential) — `MOD` is an infix modulo
//! operator (a synonym for `%`) under the MySQL dialect.
//!
//! MySQL accepts `x MOD y` as a modulo operator (`10 MOD 3` is 1,
//! `5.5 MOD 2` is 1.5), a synonym for `%`, binding at the multiplicative
//! precedence like `DIV`. SPG only had the `MOD(x, y)` function and `%`, so
//! `10 MOD 3` was a syntax error. The `MOD(x, y)` function form is
//! unaffected, and a PostgreSQL session keeps `MOD` as a function only.
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

/// Integer and decimal modulo.
#[test]
fn mod_infix() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT 10 MOD 3"), Value::Int(1));
    assert_eq!(one(&mut e, "SELECT -7 MOD 2"), Value::Int(-1));
    assert_eq!(one(&mut e, "SELECT 5.5 MOD 2"), Value::numeric(15, 1));
}

/// `MOD 0` is NULL under the MySQL dialect (like `% 0`, round 372).
#[test]
fn mod_zero_is_null() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT 10 MOD 0"), Value::Null);
}

/// `MOD` binds at the multiplicative rung (tighter than `+`).
#[test]
fn precedence() {
    let mut e = mysql();
    // 2 + (3 MOD 2) = 2 + 1 = 3
    assert_eq!(one(&mut e, "SELECT 2 + 3 MOD 2"), Value::Int(3));
    // (10 MOD 3) + 1 = 1 + 1 = 2
    assert_eq!(one(&mut e, "SELECT 10 MOD 3 + 1"), Value::Int(2));
}

/// The `MOD(x, y)` function form still works.
#[test]
fn mod_function_unaffected() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT MOD(10, 3)"), Value::Int(1));
}

/// A PostgreSQL session keeps `MOD` as a function only (the function works,
/// the infix form is not MySQL-gated there).
#[test]
fn postgres_function_only() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT MOD(10, 3)"), Value::Int(1));
    assert!(e.execute("SELECT 10 MOD 3").is_err(), "PG has no infix MOD");
}
