//! v7.37.17 (17.6 siblings) — age(ts) single-arg form. SPG anchors
//! at 2020-01-01 UTC until v7.38 threads real current_timestamp.

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

fn as_interval(v: &spg_storage::Value<'_>) -> (i32, i32, i64) {
    match v {
        spg_storage::Value::Interval {
            months,
            days,
            micros,
            kind,
        } => (*months, *days, *micros),
        other => panic!("expected Interval, got {other:?}"),
    }
}

#[test]
fn age_single_arg_from_anchor() {
    let mut e = Engine::new();
    // 2020-01-01 (anchor) — age = 0.
    let v = first(&mut e, "SELECT age('2020-01-01'::timestamp)");
    assert_eq!(as_interval(&v), (0, 0, 0));
    // 2019-12-01 → anchor - ts = 1 month (calendar diff, PG18-verified).
    let v = first(&mut e, "SELECT age('2019-12-01'::timestamp)");
    assert_eq!(as_interval(&v), (1, 0, 0));
    // 2020-02-01 → anchor - ts = -1 month.
    let v = first(&mut e, "SELECT age('2020-02-01'::timestamp)");
    assert_eq!(as_interval(&v), (-1, 0, 0));
}

#[test]
fn age_two_arg_still_works() {
    let mut e = Engine::new();
    // Regression: 2-arg form still works.
    let v = first(
        &mut e,
        "SELECT age('2020-03-01'::timestamp, '2020-01-01'::timestamp)",
    );
    // PG18: age is a calendar diff — Jan1→Mar1 is `2 mons`, not 60 days.
    assert_eq!(as_interval(&v), (2, 0, 0));
}

#[test]
fn age_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT age(NULL::timestamp)"),
        spg_storage::Value::Null
    ));
}
