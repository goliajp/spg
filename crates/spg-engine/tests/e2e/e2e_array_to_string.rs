//! v7.37.17 (17.6 siblings) — array_to_string.

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
fn array_to_string_int_basic() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_to_string(ARRAY[1, 2, 3], ',')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "1,2,3"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT array_to_string(ARRAY[1, 2, 3], ' - ')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "1 - 2 - 3"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_to_string_text_basic() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_to_string(ARRAY['a','b','c'], '.')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "a.b.c"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_to_string_empty_array_returns_empty_string() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_to_string(ARRAY[]::int[], ',')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), ""),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_to_string_null_delimiter_and_input() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_to_string(NULL::int[], ',')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT array_to_string(ARRAY[1, 2, 3], NULL::text)"),
        spg_storage::Value::Null
    ));
}
