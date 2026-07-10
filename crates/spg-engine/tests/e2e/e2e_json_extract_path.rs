//! v7.37.17 (17.6 siblings) — json_extract_path + _text variants.

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

fn any_text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn extract_path_nested_object() {
    let mut e = Engine::new();
    let v = any_text(&first(
        &mut e,
        r#"SELECT json_extract_path('{"a": {"b": {"c": 42}}}', 'a', 'b', 'c')"#,
    ));
    assert_eq!(v, "42");
}

#[test]
fn extract_path_text_strips_quotes() {
    let mut e = Engine::new();
    // _text form returns unquoted string.
    let v = any_text(&first(
        &mut e,
        r#"SELECT json_extract_path_text('{"name": "alice"}', 'name')"#,
    ));
    assert_eq!(v, "alice");
    // Non-text form keeps JSON quoting.
    let v = any_text(&first(
        &mut e,
        r#"SELECT json_extract_path('{"name": "alice"}', 'name')"#,
    ));
    assert_eq!(v, r#""alice""#);
}

#[test]
fn extract_path_array_index() {
    let mut e = Engine::new();
    let v = any_text(&first(
        &mut e,
        r#"SELECT json_extract_path('{"items": [10, 20, 30]}', 'items', '1')"#,
    ));
    assert_eq!(v, "20");
}

#[test]
fn extract_path_missing_key_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, r#"SELECT json_extract_path('{"a": 1}', 'missing')"#),
        spg_storage::Value::Null
    ));
}

#[test]
fn jsonb_variants_work() {
    let mut e = Engine::new();
    let v = any_text(&first(
        &mut e,
        r#"SELECT jsonb_extract_path_text('{"x": {"y": "z"}}', 'x', 'y')"#,
    ));
    assert_eq!(v, "z");
}

#[test]
fn extract_path_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT json_extract_path(NULL::text, 'a')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(
            &mut e,
            r#"SELECT json_extract_path('{"a": 1}', NULL::text)"#
        ),
        spg_storage::Value::Null
    ));
}
