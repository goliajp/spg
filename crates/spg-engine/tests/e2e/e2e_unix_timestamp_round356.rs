//! read01 round 356 (MySQL differential, M14) — UNIX_TIMESTAMP of a string.
//!
//! It refused every string — `unix_timestamp() needs DATE or TIMESTAMP,
//! got Some(Text)` — which is how the function is nearly always called.
//!
//! MariaDB 11, measured with the session time zone at UTC:
//! `UNIX_TIMESTAMP('2024-01-15 00:00:00')` and `('2024-01-15')` are both
//! 1705276800; a fractional part is KEPT (`'…10:30:45.5'` is
//! 1705314645.5); the bare numeric `YYYYMMDD` form works
//! (`UNIX_TIMESTAMP(20240115)`); and anything unreadable — `'not a date'`
//! or `''` — is NULL rather than an error.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new().with_clock(|| 1_784_723_696_541_528);
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
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

#[test]
fn a_date_string_is_read() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP('2024-01-15 00:00:00')"),
        Value::BigInt(1_705_276_800),
    );
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP('2024-01-15')"),
        Value::BigInt(1_705_276_800),
        "a bare date is midnight"
    );
    // …and agrees with the typed spelling that already worked.
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP(DATE '2024-01-15')"),
        Value::BigInt(1_705_276_800),
    );
}

/// The fraction is kept, not truncated.
#[test]
fn a_fractional_second_survives() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP('2024-01-15 10:30:45.5')"),
        Value::Float(1_705_314_645.5),
    );
    // A whole second stays an integer.
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP('2024-01-15 10:30:45')"),
        Value::BigInt(1_705_314_645),
    );
}

/// The bare numeric YYYYMMDD form.
#[test]
fn the_numeric_form_is_read() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT UNIX_TIMESTAMP(20240115)"),
        Value::BigInt(1_705_276_800),
    );
}

/// Unreadable input is NULL — MariaDB does not raise here.
#[test]
fn junk_is_null_not_an_error() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT UNIX_TIMESTAMP('not a date')"), Value::Null);
    assert_eq!(one(&mut e, "SELECT UNIX_TIMESTAMP('')"), Value::Null);
    assert_eq!(one(&mut e, "SELECT UNIX_TIMESTAMP(NULL)"), Value::Null);
}

/// The round trip, and the no-argument form the clock supplies.
#[test]
fn it_round_trips_through_from_unixtime() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT FROM_UNIXTIME(UNIX_TIMESTAMP('2024-01-15 10:30:45'))"),
        Value::Timestamp(1_705_314_645_000_000),
    );
    assert_eq!(one(&mut e, "SELECT UNIX_TIMESTAMP() > 0"), Value::Bool(true));
}
