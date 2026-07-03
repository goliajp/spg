//! EXTRACT / date_part field completion: DOW, ISODOW, DOY, WEEK,
//! ISOYEAR, QUARTER, DECADE, CENTURY, MILLENNIUM, JULIAN,
//! MILLISECOND, TIMEZONE — PG doc vectors.

use spg_engine::{Engine, QueryResult};

fn n(e: &mut Engine, sql: &str) -> i64 {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match rows[0].values[0] {
        spg_storage::Value::BigInt(v) => v,
        spg_storage::Value::Int(v) => i64::from(v),
        ref other => panic!("{sql}: expected integer, got {other:?}"),
    }
}

#[test]
fn day_of_week_and_year() {
    let mut e = Engine::new();
    // 2024-01-01 is a Monday.
    assert_eq!(n(&mut e, "SELECT extract(DOW FROM DATE '2024-01-01')"), 1);
    // Sundays: DOW 0, ISODOW 7.
    assert_eq!(n(&mut e, "SELECT extract(DOW FROM DATE '2024-01-07')"), 0);
    assert_eq!(n(&mut e, "SELECT extract(ISODOW FROM DATE '2024-01-07')"), 7);
    // 2024 is a leap year.
    assert_eq!(n(&mut e, "SELECT extract(DOY FROM DATE '2024-12-31')"), 366);
    assert_eq!(n(&mut e, "SELECT extract(DOY FROM DATE '2024-01-01')"), 1);
}

#[test]
fn iso_week_and_isoyear() {
    let mut e = Engine::new();
    assert_eq!(n(&mut e, "SELECT extract(WEEK FROM DATE '2024-01-01')"), 1);
    // 2023-01-01 is a Sunday — ISO week 52 of ISO year 2022.
    assert_eq!(n(&mut e, "SELECT extract(WEEK FROM DATE '2023-01-01')"), 52);
    assert_eq!(n(&mut e, "SELECT extract(ISOYEAR FROM DATE '2023-01-01')"), 2022);
    // 2020 is a 53-week ISO year.
    assert_eq!(n(&mut e, "SELECT extract(WEEK FROM DATE '2020-12-31')"), 53);
    // 2019-12-30 (Monday) already belongs to ISO week 1 of 2020.
    assert_eq!(n(&mut e, "SELECT extract(WEEK FROM DATE '2019-12-30')"), 1);
    assert_eq!(n(&mut e, "SELECT extract(ISOYEAR FROM DATE '2019-12-30')"), 2020);
}

#[test]
fn era_buckets_and_julian() {
    let mut e = Engine::new();
    assert_eq!(n(&mut e, "SELECT extract(QUARTER FROM DATE '2024-05-15')"), 2);
    assert_eq!(n(&mut e, "SELECT extract(DECADE FROM DATE '2024-01-01')"), 202);
    assert_eq!(n(&mut e, "SELECT extract(CENTURY FROM DATE '2024-01-01')"), 21);
    // Century boundary: year 2000 is still century 20.
    assert_eq!(n(&mut e, "SELECT extract(CENTURY FROM DATE '2000-01-01')"), 20);
    assert_eq!(n(&mut e, "SELECT extract(MILLENNIUM FROM DATE '2024-01-01')"), 3);
    // JD of 2000-01-01 is 2451545.
    assert_eq!(n(&mut e, "SELECT extract(JULIAN FROM DATE '2000-01-01')"), 2_451_545);
}

#[test]
fn subsecond_timezone_and_date_part() {
    let mut e = Engine::new();
    // MILLISECONDS includes the seconds: 45.5s -> 45500.
    assert_eq!(
        n(
            &mut e,
            "SELECT extract(MILLISECONDS FROM TIMESTAMP '2024-01-01 12:30:45.5')"
        ),
        45_500
    );
    // SPG sessions run UTC.
    assert_eq!(
        n(&mut e, "SELECT extract(TIMEZONE FROM TIMESTAMP '2024-01-01 00:00:00')"),
        0
    );
    // date_part shares the field table.
    assert_eq!(n(&mut e, "SELECT date_part('dow', DATE '2024-01-01')"), 1);
    assert_eq!(n(&mut e, "SELECT date_part('week', DATE '2020-12-31')"), 53);
    // Interval: QUARTER works, day-of-week honestly errors.
    assert_eq!(
        n(&mut e, "SELECT extract(QUARTER FROM INTERVAL '5 months')"),
        2
    );
    assert!(e.execute("SELECT extract(DOW FROM INTERVAL '1 day')").is_err());
}
