//! read01 round 398 (MySQL differential) — CASE / COALESCE with mixed
//! branch types coerces instead of erroring under the MySQL dialect.
//!
//! MySQL aggregates a mixed int / string CASE or COALESCE to a common type
//! (a string) rather than refusing it: `CASE WHEN 1 THEN 1 ELSE 'x' END` is
//! '1', `CASE WHEN 0 THEN 1 ELSE 'x' END` is 'x', `COALESCE(1, 'x')` is 1.
//! SPG required the untyped string literals to coerce to the resolved
//! numeric type (PG's rule) and raised "invalid input syntax for type
//! integer: 'x'", so those queries failed. A PostgreSQL session keeps the
//! strict rule; a same-type CASE is unchanged.
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

/// A taken string branch and a taken int branch both return their value.
#[test]
fn case_mixed_branches() {
    let mut e = mysql();
    // WHEN 1 true -> THEN 1 (value 1, MariaDB renders '1')
    assert_eq!(
        one(&mut e, "SELECT CASE WHEN 1 THEN 1 ELSE 'x' END"),
        Value::Int(1)
    );
    // WHEN 0 false -> ELSE 'x'
    assert_eq!(
        one(&mut e, "SELECT CASE WHEN 0 THEN 1 ELSE 'x' END"),
        Value::text("x")
    );
    // taken string branch, int else
    assert_eq!(
        one(&mut e, "SELECT CASE WHEN 1 THEN 'a' ELSE 2 END"),
        Value::text("a")
    );
}

/// COALESCE with mixed types picks the first non-NULL, no error.
#[test]
fn coalesce_mixed() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT COALESCE(1, 'x')"), Value::Int(1));
    assert_eq!(one(&mut e, "SELECT COALESCE(NULL, 'x')"), Value::text("x"));
}

/// A same-type CASE is unchanged.
#[test]
fn same_type_unchanged() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT CASE WHEN 1 THEN 1 ELSE 2 END"),
        Value::Int(1)
    );
}

/// A PostgreSQL session keeps the strict rule (refuses the mix).
#[test]
fn postgres_still_strict() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT CASE WHEN true THEN 1 ELSE 'x' END")
            .is_err(),
        "PG refuses int/text CASE branches"
    );
    assert!(
        e.execute("SELECT COALESCE(1, 'x')").is_err(),
        "PG refuses int/text COALESCE"
    );
}
