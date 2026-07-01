//! v7.37.17 (17.6 siblings) — jsonb_path_exists + jsonb_path_match.

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
fn path_exists_finds_key() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_exists('{"a": {"b": 1}}', '$.a.b')"#
    )));
    assert!(!as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_exists('{"a": 1}', '$.missing')"#
    )));
}

#[test]
fn path_exists_array_wildcard() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_exists('{"items": [1, 2, 3]}', '$.items[*]')"#
    )));
    // Empty array: wildcard matches nothing.
    assert!(!as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_exists('{"items": []}', '$.items[*]')"#
    )));
}

#[test]
fn path_match_boolean_result() {
    let mut e = Engine::new();
    // Non-empty match set → true.
    assert!(as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_match('{"a": 5}', '$.a')"#
    )));
    // No matches → false.
    assert!(!as_bool(&first(
        &mut e,
        r#"SELECT jsonb_path_match('{"a": 5}', '$.b')"#
    )));
}

#[test]
fn json_variants_work() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        r#"SELECT json_path_exists('{"x": 1}', '$.x')"#
    )));
}

#[test]
fn path_exists_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_path_exists(NULL::text, '$.a')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(
            &mut e,
            r#"SELECT jsonb_path_exists('{"a": 1}', NULL::text)"#
        ),
        spg_storage::Value::Null
    ));
}
