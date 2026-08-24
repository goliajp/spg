//! v7.37.17 (17.6 siblings) — justify_days / justify_hours /
//! justify_interval — interval canonicalizers.

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

fn assert_interval(v: &spg_storage::Value<'_>, exp_months: i32, exp_days: i32, exp_micros: i64) {
    match v {
        spg_storage::Value::Interval {
            months,
            days,
            micros,
            kind,
        } => {
            assert_eq!(*months, exp_months, "months mismatch");
            assert_eq!(*days, exp_days, "days mismatch");
            assert_eq!(*micros, exp_micros, "micros mismatch");
        }
        other => panic!("expected Interval, got {other:?}"),
    }
}

#[test]
fn justify_days_rolls_over_at_30() {
    let mut e = Engine::new();
    // 60 days → 2 months.
    assert_interval(
        &first(&mut e, "SELECT justify_days(INTERVAL '60 days')"),
        2,
        0,
        0,
    );
    // 35 days → 1 month + 5 days.
    assert_interval(
        &first(&mut e, "SELECT justify_days(INTERVAL '35 days')"),
        1,
        5,
        0,
    );
    // 15 days stays as 15 days.
    assert_interval(
        &first(&mut e, "SELECT justify_days(INTERVAL '15 days')"),
        0,
        15,
        0,
    );
}

#[test]
fn justify_hours_rolls_over_at_24() {
    let mut e = Engine::new();
    // 48 hours → 2 days.
    assert_interval(
        &first(&mut e, "SELECT justify_hours(INTERVAL '48 hours')"),
        0,
        2,
        0,
    );
    // 25 hours → 1 day + 1 hour.
    assert_interval(
        &first(&mut e, "SELECT justify_hours(INTERVAL '25 hours')"),
        0,
        1,
        3_600_000_000,
    );
}

#[test]
fn justify_interval_full_cascade() {
    let mut e = Engine::new();
    // 750 hours = 31d 6h → 1 month + 1 day + 6 hours.
    assert_interval(
        &first(&mut e, "SELECT justify_interval(INTERVAL '750 hours')"),
        1,
        1,
        6 * 3_600_000_000,
    );
}

#[test]
fn justify_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT justify_days(NULL::interval)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT justify_hours(NULL::interval)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT justify_interval(NULL::interval)"),
        spg_storage::Value::Null
    ));
}
