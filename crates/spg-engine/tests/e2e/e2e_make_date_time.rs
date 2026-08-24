//! v7.37.17 (17.6 siblings) — make_date / make_time /
//! make_timestamp / make_interval constructors.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn make_date_matches_cast() {
    let mut e = Engine::new();
    let a = first(&mut e, "SELECT make_date(2020, 1, 15)");
    let b = first(&mut e, "SELECT '2020-01-15'::date");
    match (&a, &b) {
        (spg_storage::Value::Date(x), spg_storage::Value::Date(y)) => assert_eq!(x, y),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn make_date_invalid_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT make_date(2020, 13, 1)").is_err());
    assert!(e.execute("SELECT make_date(2020, 1, 32)").is_err());
}

#[test]
fn make_timestamp_matches_cast() {
    let mut e = Engine::new();
    let a = first(&mut e, "SELECT make_timestamp(2020, 1, 15, 12, 30, 45.5)");
    let b = first(&mut e, "SELECT '2020-01-15 12:30:45.5'::timestamp");
    match (&a, &b) {
        (spg_storage::Value::Timestamp(x), spg_storage::Value::Timestamp(y)) => {
            assert_eq!(x, y)
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn make_interval_components() {
    let mut e = Engine::new();
    // 1 year, 2 months, 1 week, 3 days, 4 hours, 5 mins, 6 secs.
    match first(&mut e, "SELECT make_interval(1, 2, 1, 3, 4, 5, 6)") {
        spg_storage::Value::Interval {
            months,
            days,
            micros,
            kind,
        } => {
            assert_eq!(months, 14); // 12 + 2
            assert_eq!(days, 10); // 7 + 3
            assert_eq!(micros, 4 * 3_600_000_000i64 + 5 * 60_000_000 + 6_000_000);
        }
        other => panic!("got {other:?}"),
    }
    // Zero args → zero interval.
    match first(&mut e, "SELECT make_interval()") {
        spg_storage::Value::Interval {
            months: 0,
            days: 0,
            micros: 0,
            kind,
        } => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn make_fns_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT make_date(NULL::int, 1, 1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT make_timestamp(2020, NULL::int, 1, 0, 0, 0)"),
        spg_storage::Value::Null
    ));
}
