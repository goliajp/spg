//! read01 round 382 (MySQL differential) — EXTRACT coerces a date/time
//! STRING to its temporal value under the MySQL dialect.
//!
//! MariaDB reads `EXTRACT(YEAR FROM '2020-05-15')` as 2020 and
//! `EXTRACT(HOUR FROM '2020-05-15 10:30:00')` as 10 — the string is
//! coerced to a date/datetime. SPG required a typed source
//! (`DATE '...'` / `TIMESTAMP '...'`) and raised "EXTRACT requires DATE /
//! TIMESTAMP / INTERVAL" for a bare string, so a MySQL query extracting a
//! field from a string column/literal failed. A typed source is
//! unchanged, and a PostgreSQL session keeps the typed-source
//! requirement.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn num(e: &mut Engine, sql: &str) -> i128 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Numeric { scaled, scale: 0, .. } => *scaled,
            Value::Int(n) => i128::from(*n),
            Value::BigInt(n) => i128::from(*n),
            other => panic!("`{sql}` not an integer: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A bare date/datetime string is coerced for every field.
#[test]
fn extract_coerces_a_string() {
    let mut e = mysql();
    assert_eq!(num(&mut e, "SELECT EXTRACT(YEAR FROM '2020-05-15')"), 2020);
    assert_eq!(num(&mut e, "SELECT EXTRACT(MONTH FROM '2020-05-15')"), 5);
    assert_eq!(num(&mut e, "SELECT EXTRACT(DAY FROM '2020-05-15')"), 15);
    assert_eq!(
        num(&mut e, "SELECT EXTRACT(HOUR FROM '2020-05-15 10:30:00')"),
        10
    );
    assert_eq!(
        num(&mut e, "SELECT EXTRACT(MINUTE FROM '2020-05-15 10:30:00')"),
        30
    );
}

/// A typed source is unchanged.
#[test]
fn typed_source_unchanged() {
    let mut e = mysql();
    assert_eq!(
        num(&mut e, "SELECT EXTRACT(YEAR FROM DATE '2020-05-15')"),
        2020
    );
}

/// A PostgreSQL session still requires a typed source.
#[test]
fn postgres_requires_typed_source() {
    let mut p = Engine::new();
    assert!(p.execute("SELECT EXTRACT(YEAR FROM '2020-05-15')").is_err());
}
