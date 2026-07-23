//! read01 round 350 (MySQL differential, M7) — `INTERVAL n UNIT`.
//!
//! MySQL writes every date shift as `DATE_ADD(d, INTERVAL 1 MONTH)` or
//! `d + INTERVAL 90 MINUTE`, with the count UNQUOTED. None of it parsed.
//! PG rejects the unquoted form outright (`syntax error at or near "1"`,
//! measured), so it is taken only in the MySQL dialect; PG's own
//! `INTERVAL '1' DAY` is untouched.
//!
//! Measuring it turned up something worse and older: **a MONTH was 30
//! days** in `date_add` / `date_sub` / `adddate` / `subdate` /
//! `date_subtract`. `DATE_ADD('2024-01-31', INTERVAL 1 MONTH)` answered
//! 2024-03-01 where MariaDB 11 and PG 18.4 both answer **2024-02-29** —
//! the month clamps to the target's last day. Silently, on every
//! month-granular shift. The `+ INTERVAL` OPERATOR was always right; the
//! functions now use the same helper it does.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// 2024-02-29T00:00:00Z, as microseconds.
const FEB_29: i64 = 1_709_164_800_000_000;

fn engine(mysql: bool) -> Engine {
    let mut e = Engine::new().with_clock(|| 1_784_723_696_541_528);
    if mysql {
        e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    }
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

/// The month-end clamp, which the 30-day arithmetic got wrong in five
/// functions. Both oracles agree on every line here.
#[test]
fn a_month_is_a_calendar_month() {
    let mut e = engine(true);
    // v7.39 (round 373) — a DATE plus a day-granular interval stays a DATE
    // on MySQL (measured: MariaDB renders 2024-02-29, no time), matching
    // DATE_SUB. 19782 = 2024-02-29.
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD(DATE '2024-01-31', INTERVAL 1 MONTH)"),
        Value::Date(19782),
        "2024-01-31 + 1 month clamps to 2024-02-29, not 2024-03-01",
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_SUB(DATE '2024-03-31', INTERVAL 1 MONTH)"),
        Value::Date(19782),
        "…and 2024-03-31 - 1 month is 2024-02-29",
    );
    // The operator agrees — a DATE result, byte-for-byte with the function.
    assert_eq!(
        one(&mut e, "SELECT DATE '2024-01-31' + INTERVAL 1 MONTH"),
        Value::Date(19782),
    );
    assert_eq!(
        one(&mut e, "SELECT DATE '2024-01-31' + INTERVAL '1 month'"),
        Value::Date(19782),
    );
}

/// The unquoted form, unit by unit, against MariaDB's answers. v7.39
/// (round 373) — a DATE with a day-granular interval stays a DATE; a
/// TIMESTAMP input or a sub-day interval is a DATETIME.
#[test]
fn mysql_interval_units() {
    let mut e = engine(true);
    let cases: [(&str, Value); 5] = [
        (
            "SELECT DATE '2024-01-15' - INTERVAL 2 WEEK",
            Value::Date(19723),
        ),
        (
            "SELECT TIMESTAMP '2024-01-15 10:00:00' + INTERVAL 90 MINUTE",
            Value::Timestamp(1_705_318_200_000_000),
        ),
        (
            "SELECT DATE_ADD(DATE '2024-01-15', INTERVAL 1 QUARTER)",
            Value::Date(19828),
        ),
        (
            "SELECT DATE_ADD(TIMESTAMP '2024-01-15 10:00:00', INTERVAL 500000 MICROSECOND)",
            Value::Timestamp(1_705_312_800_500_000),
        ),
        (
            "SELECT DATE_ADD(DATE '2024-01-15', INTERVAL -1 DAY)",
            Value::Date(19736),
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(one(&mut e, sql), want, "for `{sql}`");
    }
}

/// MySQL reads a date STRING as the temporal operand, in both the
/// function and the operator spelling.
#[test]
fn a_date_string_carries_the_shift() {
    let mut e = engine(true);
    // v7.39 (round 373) — a bare date string with a day-granular interval
    // stays a DATE, matching MariaDB (19738 = 2024-01-16, 19782 =
    // 2024-02-29, 19739 = 2024-01-17).
    assert_eq!(
        one(&mut e, "SELECT '2024-01-15' + INTERVAL 1 DAY"),
        Value::Date(19738),
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2024-01-31', INTERVAL 1 MONTH)"),
        Value::Date(19782),
    );
    assert_eq!(
        one(&mut e, "SELECT DATE_ADD('2024-01-15', INTERVAL '2' DAY)"),
        Value::Date(19739),
    );
}

/// PG has no unquoted form — its own spellings keep working, and the
/// unquoted one stays a syntax error there.
#[test]
fn pg_keeps_its_own_spellings() {
    let mut e = engine(false);
    assert_eq!(
        one(&mut e, "SELECT DATE '2024-01-31' + INTERVAL '1 month'"),
        Value::Timestamp(FEB_29),
    );
    assert_eq!(
        one(&mut e, "SELECT DATE '2024-01-15' + INTERVAL '1' DAY"),
        Value::Timestamp(1_705_363_200_000_000),
    );
    assert!(
        e.execute("SELECT DATE '2024-01-15' + INTERVAL 1 DAY").is_err(),
        "PG: syntax error at or near \"1\"",
    );
}
