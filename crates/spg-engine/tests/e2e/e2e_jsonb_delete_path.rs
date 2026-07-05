//! v7.37.17 (17.6 siblings) — jsonb_delete_path (#- function form).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn json(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Json(s) => s.to_string(),
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn delete_path_nested_object_key() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"a": {"b": 1, "c": 2}}', '{a,b}')"#
        )),
        r#"{"a": {"c": 2}}"#
    );
}

#[test]
fn delete_path_array_element() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"items": [10, 20, 30]}', '{items,1}')"#
        )),
        r#"{"items": [10, 30]}"#
    );
    // Negative index.
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"items": [10, 20, 30]}', '{items,-1}')"#
        )),
        r#"{"items": [10, 20]}"#
    );
}

#[test]
fn delete_path_missing_leaves_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"a": 1}', '{zzz,deep}')"#
        )),
        r#"{"a": 1}"#
    );
}

#[test]
fn delete_path_top_level() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"a": 1, "b": 2}', '{a}')"#
        )),
        r#"{"b": 2}"#
    );
}

#[test]
fn delete_path_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_delete_path(NULL::text, '{a}')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(
            &mut e,
            r#"SELECT jsonb_delete_path('{"a": 1}', NULL::text)"#
        ),
        spg_storage::Value::Null
    ));
}
