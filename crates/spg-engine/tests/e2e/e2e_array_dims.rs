//! v7.37.17 (17.6 siblings) — array_upper / array_lower /
//! array_ndims / array_dims.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn array_upper_lower_1d() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_lower(ARRAY[10, 20, 30], 1)") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT array_upper(ARRAY[10, 20, 30], 1)") {
        spg_storage::Value::Int(3) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_upper_lower_null_for_other_dims() {
    let mut e = Engine::new();
    // SPG models 1-D arrays only; dim=2 → NULL.
    assert!(matches!(
        first(&mut e, "SELECT array_upper(ARRAY[1, 2, 3], 2)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT array_lower(ARRAY[1, 2, 3], 2)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn array_ndims_returns_1_for_non_empty() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_ndims(ARRAY[1, 2, 3])") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT array_ndims(ARRAY['a', 'b'])") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_dims_returns_bracket_range() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_dims(ARRAY[1, 2, 3, 4, 5])") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "[1:5]"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT array_dims(ARRAY['x'])") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "[1:1]"),
        other => panic!("got {other:?}"),
    }
}
