//! v7.37.17 (17.6 siblings) — to_timestamp(double) — Unix epoch
//! seconds → timestamp.

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
fn to_timestamp_from_epoch_zero() {
    let mut e = Engine::new();
    // 1970-01-01 00:00:00 UTC = 0 seconds.
    let expected = ts(&first(&mut e, "SELECT '1970-01-01 00:00:00'::timestamp"));
    assert_eq!(ts(&first(&mut e, "SELECT to_timestamp(0)")), expected);
}

#[test]
fn to_timestamp_from_epoch_2020() {
    let mut e = Engine::new();
    // 2020-01-01 00:00:00 UTC = 1577836800 seconds.
    let expected = ts(&first(&mut e, "SELECT '2020-01-01 00:00:00'::timestamp"));
    assert_eq!(
        ts(&first(&mut e, "SELECT to_timestamp(1577836800)")),
        expected
    );
}

#[test]
fn to_timestamp_from_fractional_seconds() {
    let mut e = Engine::new();
    // 1577836800.5 → 2020-01-01 00:00:00.500000
    let expected = ts(&first(
        &mut e,
        "SELECT '2020-01-01 00:00:00.500000'::timestamp",
    ));
    assert_eq!(
        ts(&first(&mut e, "SELECT to_timestamp(1577836800.5)")),
        expected
    );
}

#[test]
fn to_timestamp_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_timestamp(NULL::float)"),
        spg_storage::Value::Null
    ));
}
