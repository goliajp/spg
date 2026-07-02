//! v7.37.17 (17.6 siblings) — isfinite(date|timestamp|interval|float).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_bool(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn isfinite_date_timestamp_interval() {
    let mut e = Engine::new();
    assert!(as_bool(&first(&mut e, "SELECT isfinite('2020-01-01'::date)")));
    assert!(as_bool(&first(
        &mut e,
        "SELECT isfinite('2020-01-01 12:00:00'::timestamp)"
    )));
    assert!(as_bool(&first(
        &mut e,
        "SELECT isfinite(INTERVAL '1 day')"
    )));
}

#[test]
fn isfinite_float_infinity() {
    let mut e = Engine::new();
    assert!(as_bool(&first(&mut e, "SELECT isfinite(1.5)")));
    // Infinity via division overflow shape.
    assert!(!as_bool(&first(
        &mut e,
        "SELECT isfinite('Infinity'::float)"
    )));
}

#[test]
fn isfinite_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT isfinite(NULL::timestamp)"),
        spg_storage::Value::Null
    ));
}
