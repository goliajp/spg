//! read01 round 373 (MySQL differential) — `DATE_ADD` / `date + INTERVAL`
//! keep a DATE result under the MySQL dialect when the interval is
//! day-granular; a time component lifts to DATETIME.
//!
//! MariaDB 11: `DATE_ADD('2020-01-31', INTERVAL 1 MONTH)` is the DATE
//! 2020-02-29 (rendered without a time), while `INTERVAL 1 HOUR` gives
//! the DATETIME `2020-01-31 01:00:00`. SPG followed PG — `date + interval`
//! is always TIMESTAMP — so the `DATE_ADD` function and the `+ INTERVAL`
//! operator both handed back a midnight datetime for a plain date shift,
//! diverging from MySQL's rendered type. `DATE_SUB` / `ADDDATE` already
//! kept the DATE; the `DATE_ADD` function and the operator did not.
//!
//! The value was always right — this pins the result TYPE. A PostgreSQL
//! session keeps `date + interval` = timestamp.

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

/// A day-granular DATE_ADD keeps a DATE (value 2020-02-29 = day 18321).
#[test]
fn date_add_day_granular_stays_date() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT DATE_ADD('2020-01-31', INTERVAL 1 MONTH)"),
        Value::Date(18321)
    );
    assert_eq!(
        scalar(&mut e, "SELECT DATE_ADD('2020-01-31', INTERVAL 1 DAY)"),
        Value::Date(18293)
    );
    assert_eq!(
        scalar(&mut e, "SELECT DATE_ADD('2020-01-31', INTERVAL 1 YEAR)"),
        Value::Date(18658)
    );
}

/// The `+ INTERVAL` operator and DATE_SUB behave the same.
#[test]
fn operator_and_sub_stay_date() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT DATE '2020-01-31' + INTERVAL 1 MONTH"),
        Value::Date(18321)
    );
    assert_eq!(
        scalar(&mut e, "SELECT DATE_SUB('2020-03-01', INTERVAL 1 DAY)"),
        Value::Date(18321)
    );
}

/// A time-granular interval, or a TIMESTAMP input, lifts to DATETIME.
#[test]
fn time_component_lifts_to_timestamp() {
    let mut e = mysql();
    assert!(matches!(
        scalar(&mut e, "SELECT DATE_ADD('2020-01-31', INTERVAL 1 HOUR)"),
        Value::Timestamp(_)
    ));
    assert!(matches!(
        scalar(
            &mut e,
            "SELECT DATE_ADD(TIMESTAMP '2020-01-31 12:00:00', INTERVAL 1 MONTH)"
        ),
        Value::Timestamp(_)
    ));
}

/// A PostgreSQL session keeps `date + interval` = timestamp.
#[test]
fn postgres_session_lifts_to_timestamp() {
    let mut p = Engine::new();
    assert!(matches!(
        scalar(&mut p, "SELECT DATE '2020-01-31' + INTERVAL '1 month'"),
        Value::Timestamp(_)
    ));
}
