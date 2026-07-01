//! v7.37.17 (17.6 siblings) — PG 14+ date_bin(stride, ts, origin).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn date_bin_15_minute_stride() {
    let mut e = Engine::new();
    let expected = ts(&first(&mut e, "SELECT '2020-01-01 00:30:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_bin(INTERVAL '15 minutes', \
         '2020-01-01 00:37:00'::timestamp, \
         '2020-01-01 00:00:00'::timestamp)",
    ));
    assert_eq!(got, expected);
}

#[test]
fn date_bin_1_hour_stride() {
    let mut e = Engine::new();
    let expected = ts(&first(&mut e, "SELECT '2020-01-01 03:00:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_bin(INTERVAL '1 hour', \
         '2020-01-01 03:45:00'::timestamp, \
         '2020-01-01 00:00:00'::timestamp)",
    ));
    assert_eq!(got, expected);
}

#[test]
fn date_bin_1_day_stride() {
    let mut e = Engine::new();
    let expected = ts(&first(&mut e, "SELECT '2020-01-15 00:00:00'::timestamp"));
    let got = ts(&first(
        &mut e,
        "SELECT date_bin(INTERVAL '1 day', \
         '2020-01-15 14:30:00'::timestamp, \
         '2020-01-01 00:00:00'::timestamp)",
    ));
    assert_eq!(got, expected);
}

#[test]
fn date_bin_negative_stride_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute(
            "SELECT date_bin(INTERVAL '-1 hour', \
             '2020-01-01'::timestamp, '2020-01-01'::timestamp)"
        )
        .is_err()
    );
}

#[test]
fn date_bin_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(
            &mut e,
            "SELECT date_bin(NULL::interval, \
             '2020-01-01'::timestamp, '2020-01-01'::timestamp)"
        ),
        spg_storage::Value::Null
    ));
}
