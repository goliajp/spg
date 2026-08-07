//! read01 round 418 (MySQL differential) — datetime composition / extraction:
//! `TIMESTAMP(date, time)`, datetime-aware `ADDTIME` / `SUBTIME` / `TIMEDIFF`,
//! and the compound `EXTRACT` units.
//!
//! Three loud-error clusters in the same family, all rejected by SPG:
//!   * `TIMESTAMP('2020-01-05','01:30:45')` — "function timestamp(text,text)
//!     does not exist". MySQL composes a DATETIME; the time may be negative
//!     or exceed 24h, rolling the date.
//!   * `ADDTIME('2020-01-05 10:00:00','01:30:00')` — "invalid time". SPG read
//!     every operand as a time-of-day, so a DATETIME operand errored. MySQL
//!     returns a DATETIME (with day rollover), and `TIMEDIFF` over two
//!     DATETIMEs is their full difference as a TIME.
//!   * `EXTRACT(DAY_SECOND FROM …)` — "unit not recognized". MySQL packs
//!     several components into ONE integer by decimal concatenation.
//!
//! A PostgreSQL session keeps all three errors (PG has none of these forms).
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

/// `TIMESTAMP(date, time)` composes a DATETIME; the time may roll the date.
#[test]
fn timestamp_two_arg_composes() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05','01:30:45')"),
        "2020-01-05 01:30:45"
    );
    // A negative time rolls back a day.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05','-01:00:00')"),
        "2020-01-04 23:00:00"
    );
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05','-25:00:00')"),
        "2020-01-03 23:00:00"
    );
    // A time past 24h rolls forward.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05','25:00:00')"),
        "2020-01-06 01:00:00"
    );
    // Adding to an existing DATETIME accumulates.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05 10:00:00','01:30:00')"),
        "2020-01-05 11:30:00"
    );
    // NULL either side.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05', NULL)"),
        "NULL"
    );
    // The 1-arg cast spelling is unaffected.
    assert_eq!(
        scalar(&mut e, "SELECT TIMESTAMP('2020-01-05')"),
        "2020-01-05 00:00:00"
    );
}

/// ADDTIME / SUBTIME with a DATETIME first operand return a DATETIME.
#[test]
fn addtime_subtime_on_datetime() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT ADDTIME('2020-01-05 10:00:00','01:30:00')"),
        "2020-01-05 11:30:00"
    );
    // Day rollover.
    assert_eq!(
        scalar(&mut e, "SELECT ADDTIME('2020-01-05 23:00:00','02:00:00')"),
        "2020-01-06 01:00:00"
    );
    // A negative time operand subtracts.
    assert_eq!(
        scalar(&mut e, "SELECT ADDTIME('2020-01-05 10:00:00','-01:30:00')"),
        "2020-01-05 08:30:00"
    );
    // Fractional seconds pad to six digits, as MariaDB renders them.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT ADDTIME('2020-01-05 10:00:00','00:00:01.500000')"
        ),
        "2020-01-05 10:00:01.500000"
    );
    assert_eq!(
        scalar(&mut e, "SELECT SUBTIME('2020-01-05 10:00:00','01:30:00')"),
        "2020-01-05 08:30:00"
    );
}

/// The time-only forms are unchanged.
#[test]
fn addtime_subtime_on_time_unchanged() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT ADDTIME('01:00:00','00:30:00')"),
        "01:30:00"
    );
    assert_eq!(
        scalar(&mut e, "SELECT SUBTIME('01:00:00','00:30:00')"),
        "00:30:00"
    );
}

/// TIMEDIFF over two DATETIMEs is their full difference (crossing midnight);
/// over two TIMEs it is the signed time difference. Mixed shapes are NULL.
#[test]
fn timediff_shapes() {
    let mut e = mysql();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT TIMEDIFF('2020-01-06 01:00:00','2020-01-05 23:00:00')"
        ),
        "02:00:00"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT TIMEDIFF('2020-01-05 12:00:00','2020-01-05 10:00:00')"
        ),
        "02:00:00"
    );
    assert_eq!(
        scalar(&mut e, "SELECT TIMEDIFF('10:00:00','12:30:00')"),
        "-02:30:00"
    );
    // datetime vs bare time -> NULL.
    assert_eq!(
        scalar(&mut e, "SELECT TIMEDIFF('2020-01-05 10:00:00','01:00:00')"),
        "NULL"
    );
}

/// The compound EXTRACT units pack components by decimal concatenation.
#[test]
fn compound_extract_units() {
    let mut e = mysql();
    let ts = "'2020-03-05 10:30:45'";
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(YEAR_MONTH FROM {ts})")),
        "202003"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(DAY_HOUR FROM {ts})")),
        "510"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(DAY_MINUTE FROM {ts})")),
        "51030"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(DAY_SECOND FROM {ts})")),
        "5103045"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(HOUR_MINUTE FROM {ts})")),
        "1030"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(HOUR_SECOND FROM {ts})")),
        "103045"
    );
    assert_eq!(
        scalar(&mut e, &format!("SELECT EXTRACT(MINUTE_SECOND FROM {ts})")),
        "3045"
    );
}

/// The microsecond-tailed compound units.
#[test]
fn compound_extract_microsecond_units() {
    let mut e = mysql();
    let ts = "'2020-03-05 10:30:45.123456'";
    assert_eq!(
        scalar(
            &mut e,
            &format!("SELECT EXTRACT(DAY_MICROSECOND FROM {ts})")
        ),
        "5103045123456"
    );
    assert_eq!(
        scalar(
            &mut e,
            &format!("SELECT EXTRACT(HOUR_MICROSECOND FROM {ts})")
        ),
        "103045123456"
    );
    assert_eq!(
        scalar(
            &mut e,
            &format!("SELECT EXTRACT(MINUTE_MICROSECOND FROM {ts})")
        ),
        "3045123456"
    );
    assert_eq!(
        scalar(
            &mut e,
            &format!("SELECT EXTRACT(SECOND_MICROSECOND FROM {ts})")
        ),
        "45123456"
    );
}

/// A bare DATE has a zero time-of-day; single-digit components do not pad.
#[test]
fn compound_extract_edges() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT EXTRACT(DAY_SECOND FROM '2020-03-05')"),
        "5000000"
    );
    // hour 0, minute 5 -> 0*100 + 5 = 5 (no zero padding in the packed int).
    assert_eq!(
        scalar(
            &mut e,
            "SELECT EXTRACT(HOUR_MINUTE FROM '2020-03-05 00:05:00')"
        ),
        "5"
    );
    // day 5, hour 1 -> 501.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT EXTRACT(DAY_HOUR FROM '2020-03-05 01:30:45')"
        ),
        "501"
    );
}

/// A PostgreSQL session rejects all three MySQL-only forms, and its own
/// EXTRACT / date_part are untouched.
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT EXTRACT(YEAR_MONTH FROM TIMESTAMP '2020-03-05 10:30:45')")
            .is_err(),
        "PG has no compound EXTRACT units"
    );
    assert!(
        e.execute("SELECT timestamp('2020-01-05','01:30:45')")
            .is_err(),
        "PG has no 2-arg timestamp()"
    );
    // PG's own units still work.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT EXTRACT(YEAR FROM TIMESTAMP '2020-03-05 10:30:45')"
        ),
        "2020"
    );
    assert_eq!(
        scalar(&mut e, "SELECT date_part('year', TIMESTAMP '2020-03-05')"),
        "2020"
    );
}
