//! v7.37.17 (17.6 siblings) — PG 16+ date_add / date_subtract.

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

fn ts(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Timestamp(t) => *t,
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

#[test]
fn date_add_hour_interval() {
    let mut e = Engine::new();
    // '2020-01-01 00:00:00' + 1 hour = '2020-01-01 01:00:00'
    let base = ts(&first(&mut e, "SELECT '2020-01-01 00:00:00'::timestamp"));
    let expected = ts(&first(&mut e, "SELECT '2020-01-01 01:00:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_add('2020-01-01 00:00:00'::timestamp, INTERVAL '1 hour')",
    ));
    assert_eq!(got - base, 3_600_000_000);
    assert_eq!(got, expected);
}

#[test]
fn date_add_day_interval_on_date() {
    let mut e = Engine::new();
    let expected = ts(&first(&mut e, "SELECT '2020-01-02 00:00:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_add('2020-01-01'::date, INTERVAL '1 day')",
    ));
    assert_eq!(got, expected);
}

#[test]
fn date_subtract_days() {
    let mut e = Engine::new();
    let expected = ts(&first(&mut e, "SELECT '2020-01-01 00:00:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_subtract('2020-01-10 00:00:00'::timestamp, INTERVAL '9 days')",
    ));
    assert_eq!(got, expected);
}

#[test]
fn date_add_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT date_add(NULL::timestamp, INTERVAL '1 day')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(
            &mut e,
            "SELECT date_add('2020-01-01'::timestamp, NULL::interval)"
        ),
        spg_storage::Value::Null
    ));
}
