//! v7.37.17 (17.6 siblings) — jsonb_strip_nulls.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_json<'a>(v: &'a spg_storage::Value<'a>) -> &'a str {
    match v {
        spg_storage::Value::Json(s) => s.as_ref(),
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn strips_null_keys_from_object() {
    let mut e = Engine::new();
    let v = first(
        &mut e,
        "SELECT jsonb_strip_nulls('{\"a\":1,\"b\":null,\"c\":2}'::jsonb)",
    );
    assert_eq!(as_json(&v), r#"{"a":1,"c":2}"#);
}

#[test]
fn keeps_null_array_items() {
    let mut e = Engine::new();
    let v = first(
        &mut e,
        "SELECT jsonb_strip_nulls('[1, null, 2]'::jsonb)",
    );
    assert_eq!(as_json(&v), r#"[1,null,2]"#);
}

#[test]
fn recurses_into_nested_objects() {
    let mut e = Engine::new();
    let v = first(
        &mut e,
        "SELECT jsonb_strip_nulls('{\"a\":{\"x\":null,\"y\":1},\"b\":null}'::jsonb)",
    );
    assert_eq!(as_json(&v), r#"{"a":{"y":1}}"#);
}

#[test]
fn no_nulls_unchanged() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT jsonb_strip_nulls('{\"a\":1,\"b\":2}'::jsonb)");
    assert_eq!(as_json(&v), r#"{"a":1,"b":2}"#);
}

#[test]
fn null_input_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_strip_nulls(NULL::jsonb)"),
        spg_storage::Value::Null
    ));
}
