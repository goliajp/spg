//! read01 round 414 (MySQL differential) — implicit integer → DATE / TIMESTAMP
//! for every date-family function and for date + interval arithmetic.
//!
//! MySQL reads a plain integer as a temporal value when a date-family
//! function (or a `date ± INTERVAL` operator) sees one, following the digit
//! shape:
//!   * 8 digits YYYYMMDD → DATE
//!   * 6 digits YYMMDD    → DATE (YY < 70 → 20YY, else 19YY)
//!   * 14 digits YYYYMMDDHHMMSS → TIMESTAMP
//!   * 12 digits YYMMDDHHMMSS   → TIMESTAMP (same YY rule)
//! An unrecognised shape or an invalid date (`DATE(202005)`, `YEAR(69)`)
//! returns NULL. SPG errored out with `needs date, got Some(Int)` at every
//! entry — a loud-error cluster of 15+ functions that killed common MySQL
//! query patterns (YYYYMMDD is MariaDB's own doc form for a date literal).
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

/// Every extraction function reads an 8-digit YYYYMMDD as a DATE.
#[test]
fn extraction_functions_read_yyyymmdd() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT YEAR(20200105)"), "2020");
    assert_eq!(scalar(&mut e, "SELECT MONTH(20200105)"), "1");
    assert_eq!(scalar(&mut e, "SELECT DAY(20200105)"), "5");
    assert_eq!(scalar(&mut e, "SELECT QUARTER(20200105)"), "1");
    assert_eq!(scalar(&mut e, "SELECT DAYNAME(20200105)"), "Sunday");
    assert_eq!(scalar(&mut e, "SELECT MONTHNAME(20200105)"), "January");
    assert_eq!(scalar(&mut e, "SELECT DAYOFWEEK(20200105)"), "1");
    assert_eq!(scalar(&mut e, "SELECT WEEKDAY(20200105)"), "6");
}

/// DATE / TIMESTAMP casts and the DATE_ADD / DATE_SUB / DATEDIFF /
/// DATE_FORMAT family all accept an integer date.
#[test]
fn cast_and_arithmetic_functions() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT DATE(20200105)"), "2020-01-05");
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP(20200105)"),
        "2020-01-05 00:00:00"
    );
    assert_eq!(
        scalar(&mut e, "SELECT DATE_ADD(20200101, INTERVAL 1 DAY)"),
        "2020-01-02"
    );
    assert_eq!(
        scalar(&mut e, "SELECT DATE_SUB(20200105, INTERVAL 1 DAY)"),
        "2020-01-04"
    );
    assert_eq!(scalar(&mut e, "SELECT DATEDIFF(20200110, 20200105)"), "5");
    assert_eq!(
        scalar(&mut e, "SELECT DATE_FORMAT(20200105, '%Y-%m-%d')"),
        "2020-01-05"
    );
}

/// 6-digit YYMMDD honours the YY < 70 → 20YY / else 19YY cutoff.
#[test]
fn yymmdd_century_cutoff() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT DATE(200105)"), "2020-01-05");
    assert_eq!(scalar(&mut e, "SELECT DATE(690101)"), "2069-01-01");
    assert_eq!(scalar(&mut e, "SELECT DATE(700101)"), "1970-01-01");
    assert_eq!(scalar(&mut e, "SELECT DATE(991231)"), "1999-12-31");
}

/// 14-digit YYYYMMDDHHMMSS reads as a full TIMESTAMP.
#[test]
fn yyyymmddhhmmss_lifts_to_timestamp() {
    let mut e = mysql();
    // TIMESTAMP() returns the timestamp; DATE() strips the time.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP(20200105013045)"),
        "2020-01-05 01:30:45"
    );
    assert_eq!(scalar(&mut e, "SELECT DATE(20200105013045)"), "2020-01-05");
}

/// A malformed integer shape returns NULL (MariaDB's own reading).
#[test]
fn malformed_int_returns_null() {
    let mut e = mysql();
    // 6-digit but month 20 is invalid -> NULL.
    assert_eq!(scalar(&mut e, "SELECT DATE(202005)"), "NULL");
    // month 13 -> NULL.
    assert_eq!(scalar(&mut e, "SELECT DATE(20201302)"), "NULL");
    // negative -> NULL.
    assert_eq!(scalar(&mut e, "SELECT DATE(-20200105)"), "NULL");
    // Too few digits to be any shape -> NULL.
    assert_eq!(scalar(&mut e, "SELECT YEAR(69)"), "NULL");
    // NULL propagates.
    assert_eq!(scalar(&mut e, "SELECT DATE(NULL)"), "NULL");
}

/// `int ± INTERVAL day` also reads the integer as a DATE (via the date +
/// interval binary op, MySQL only).
#[test]
fn int_plus_interval_day() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT 200101 + INTERVAL 1 DAY"),
        "2020-01-02"
    );
    assert_eq!(
        scalar(&mut e, "SELECT 20200105 - INTERVAL 1 DAY"),
        "2020-01-04"
    );
}

/// A PostgreSQL session keeps the strict type error (integer never reads as
/// a date).
#[test]
fn postgres_rejects() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT year(20200105)").is_err(),
        "PG rejects an integer to YEAR"
    );
    assert!(
        e.execute("SELECT date(20200105)").is_err(),
        "PG rejects an integer to DATE"
    );
}
