//! v7.37.17 (17.6 siblings) — array_to_json.

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
fn array_to_json_int_array() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_to_json(ARRAY[1, 2, 3])");
    assert_eq!(as_json(&v), "[1,2,3]");
}

#[test]
fn array_to_json_text_array_escapes_quotes() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_to_json(ARRAY['a', 'b', 'c'])");
    assert_eq!(as_json(&v), r#"["a","b","c"]"#);
}

#[test]
fn array_to_json_empty() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_to_json(ARRAY[]::int[])");
    assert_eq!(as_json(&v), "[]");
}

#[test]
fn array_to_json_pretty() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_to_json(ARRAY[1, 2, 3], true)");
    assert_eq!(as_json(&v), "[\n 1,\n 2,\n 3\n]");
}

#[test]
fn array_to_json_null_input() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_to_json(NULL::int[])"),
        spg_storage::Value::Null
    ));
}
