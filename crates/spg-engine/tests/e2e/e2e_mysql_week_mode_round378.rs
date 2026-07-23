//! read01 round 378 (MySQL differential) — WEEK / YEARWEEK honour the
//! mode argument (0..7); WEEKOFYEAR is the ISO week.
//!
//! MariaDB 11's `WEEK(date, mode)` picks a first-day-of-week (Sunday /
//! Monday), a 0-53 vs 1-53 range, and a week-1 rule from the mode's bits:
//! `WEEK('2020-01-01', 0)` is 0 but `WEEK('2020-01-01', 1)` is 1, and
//! `WEEK('2020-01-01', 2)` is 52. SPG ignored the mode and always
//! computed mode 0, so every non-default WEEK / YEARWEEK was silently
//! wrong — a real hazard for week-bucketed reporting. WEEKOFYEAR is
//! `WEEK(date, 3)` (ISO-8601), and YEARWEEK always uses the with-year
//! reckoning, returning year*100 + week. PG has no WEEK mode.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn int(e: &mut Engine, sql: &str) -> i32 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::Int(n) => n,
            ref other => panic!("`{sql}` not int: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// WEEK across all eight modes for a leap-year Jan 1 (a Wednesday).
#[test]
fn week_all_modes() {
    let mut e = mysql();
    let got: Vec<i32> = (0..8)
        .map(|m| int(&mut e, &format!("SELECT WEEK('2020-01-01', {m})")))
        .collect();
    assert_eq!(got, vec![0, 1, 52, 1, 1, 0, 1, 52]);
}

/// The year-boundary cases (2021-01-01 is a Friday; mid- and end-of-year).
#[test]
fn week_year_boundaries() {
    let mut e = mysql();
    assert_eq!(int(&mut e, "SELECT WEEK('2021-01-01', 0)"), 0);
    assert_eq!(int(&mut e, "SELECT WEEK('2021-01-01', 2)"), 52);
    assert_eq!(int(&mut e, "SELECT WEEK('2021-01-01', 3)"), 53);
    assert_eq!(int(&mut e, "SELECT WEEK('2020-06-15', 1)"), 25);
    assert_eq!(int(&mut e, "SELECT WEEK('2020-12-31', 1)"), 53);
    // The default (no mode) is mode 0.
    assert_eq!(int(&mut e, "SELECT WEEK('2020-06-15')"), 24);
}

/// YEARWEEK returns year*100 + week and rolls a leading partial week into
/// the previous year; WEEKOFYEAR is the ISO week.
#[test]
fn yearweek_and_weekofyear() {
    let mut e = mysql();
    assert_eq!(int(&mut e, "SELECT YEARWEEK('2020-01-01', 0)"), 201952);
    assert_eq!(int(&mut e, "SELECT YEARWEEK('2020-01-01', 1)"), 202001);
    assert_eq!(int(&mut e, "SELECT YEARWEEK('2021-01-01', 1)"), 202053);
    assert_eq!(int(&mut e, "SELECT WEEKOFYEAR('2020-01-01')"), 1);
    assert_eq!(int(&mut e, "SELECT WEEKOFYEAR('2021-01-01')"), 53);
}
