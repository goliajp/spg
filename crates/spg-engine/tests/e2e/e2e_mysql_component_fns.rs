//! v7.37.17 (17.6 siblings) — MySQL bare component accessors:
//! day / month / year / hour / minute / second / weekday / week +
//! period_add / period_diff.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn date_components() {
    let mut e = Engine::new();
    assert_eq!(int(&first(&mut e, "SELECT day('2007-02-03')")), 3);
    assert_eq!(int(&first(&mut e, "SELECT dayofmonth('2007-02-03')")), 3);
    assert_eq!(int(&first(&mut e, "SELECT month('2007-02-03')")), 2);
    assert_eq!(int(&first(&mut e, "SELECT year('2007-02-03')")), 2007);
}

#[test]
fn weekday_and_week() {
    let mut e = Engine::new();
    // MySQL WEEKDAY: 0=Monday. 2007-02-03 was Saturday → 5.
    assert_eq!(int(&first(&mut e, "SELECT weekday('2007-02-03')")), 5);
    // WEEK mode 0: 2008-02-20 → 7 (MySQL doc vector).
    assert_eq!(int(&first(&mut e, "SELECT week('2008-02-20')")), 7);
    // Days before the year's first Sunday are week 0.
    assert_eq!(int(&first(&mut e, "SELECT week('1987-01-01')")), 0);
}

#[test]
fn time_components() {
    let mut e = Engine::new();
    // MySQL doc vector: HOUR('10:05:03') = 10.
    assert_eq!(int(&first(&mut e, "SELECT hour('10:05:03')")), 10);
    assert_eq!(int(&first(&mut e, "SELECT minute('10:05:03')")), 5);
    assert_eq!(int(&first(&mut e, "SELECT second('10:05:03')")), 3);
    // TIME values past 24h: HOUR('272:59:59') = 272.
    assert_eq!(int(&first(&mut e, "SELECT hour('272:59:59')")), 272);
}

#[test]
fn period_arithmetic() {
    let mut e = Engine::new();
    // MySQL doc vectors: PERIOD_ADD(200801, 2) = 200803;
    // PERIOD_DIFF(200802, 200703) = 11.
    assert!(matches!(
        first(&mut e, "SELECT period_add(200801, 2)"),
        spg_storage::Value::BigInt(200_803)
    ));
    assert!(matches!(
        first(&mut e, "SELECT period_diff(200802, 200703)"),
        spg_storage::Value::BigInt(11)
    ));
    // Crossing a year boundary backwards.
    assert!(matches!(
        first(&mut e, "SELECT period_add(200801, -2)"),
        spg_storage::Value::BigInt(200_711)
    ));
}

#[test]
fn component_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "day(NULL::text)",
        "month(NULL::text)",
        "hour(NULL::text)",
        "week(NULL::text)",
        "period_add(NULL::int, 1)",
        "period_diff(200801, NULL::int)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
