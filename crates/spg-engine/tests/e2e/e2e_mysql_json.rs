//! v7.37.17 (17.6 siblings) — MySQL non-path JSON functions,
//! batch 1: json_valid / json_type / json_length / json_keys /
//! json_depth / json_quote / json_unquote / json_array /
//! json_pretty / json_storage_size.

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn json_valid_and_type() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT json_valid('{\"a\": 1}')"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_valid('not json')"),
        spg_storage::Value::Bool(false)
    ));
    // MySQL type names: uppercase + INTEGER/DOUBLE distinction.
    assert_eq!(text(&first(&mut e, "SELECT json_type('{}')")), "OBJECT");
    assert_eq!(text(&first(&mut e, "SELECT json_type('[]')")), "ARRAY");
    assert_eq!(text(&first(&mut e, "SELECT json_type('3')")), "INTEGER");
    assert_eq!(text(&first(&mut e, "SELECT json_type('3.5')")), "DOUBLE");
    assert_eq!(text(&first(&mut e, "SELECT json_type('true')")), "BOOLEAN");
}

#[test]
fn json_length_and_depth() {
    let mut e = Engine::new();
    // MySQL doc vectors: JSON_LENGTH('[1, 2, {"a": 3}]') → 3;
    // JSON_LENGTH('{"a": 1, "b": {"c": 30}}') → 2; scalar → 1.
    assert!(matches!(
        first(&mut e, "SELECT json_length('[1, 2, {\"a\": 3}]')"),
        spg_storage::Value::Int(3)
    ));
    assert!(matches!(
        first(
            &mut e,
            "SELECT json_length('{\"a\": 1, \"b\": {\"c\": 30}}')"
        ),
        spg_storage::Value::Int(2)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_length('1')"),
        spg_storage::Value::Int(1)
    ));
    // MySQL doc vectors: JSON_DEPTH('{}') → 1; '[10, 20]' → 2;
    // '[10, {"a": 20}]' → 3.
    assert!(matches!(
        first(&mut e, "SELECT json_depth('{}')"),
        spg_storage::Value::Int(1)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_depth('[10, 20]')"),
        spg_storage::Value::Int(2)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_depth('[10, {\"a\": 20}]')"),
        spg_storage::Value::Int(3)
    ));
}

#[test]
fn json_keys_quote_unquote() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_KEYS('{"a": 1, "b": {"c": 30}}')
    // → ["a", "b"].
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_keys('{\"a\": 1, \"b\": {\"c\": 30}}')"
        )),
        "[\"a\", \"b\"]"
    );
    // Non-object → NULL.
    assert!(matches!(
        first(&mut e, "SELECT json_keys('[1, 2]')"),
        spg_storage::Value::Null
    ));
    // MySQL doc vector: JSON_QUOTE('null') → '"null"'.
    assert_eq!(
        text(&first(&mut e, "SELECT json_quote('null')")),
        "\"null\""
    );
    // MySQL doc vector: JSON_UNQUOTE('"abc"') → 'abc'.
    assert_eq!(
        text(&first(&mut e, "SELECT json_unquote('\"abc\"')")),
        "abc"
    );
    // Unquoted input passes through.
    assert_eq!(text(&first(&mut e, "SELECT json_unquote('[1]')")), "[1]");
}

#[test]
fn json_array_and_storage_size() {
    let mut e = Engine::new();
    let arr = text(&first(&mut e, "SELECT json_array(1, 'abc', NULL)"));
    assert!(
        arr.contains('1') && arr.contains("abc") && arr.contains("null"),
        "unexpected: {arr}"
    );
    assert!(matches!(
        first(&mut e, "SELECT json_storage_size('[1,2]')"),
        spg_storage::Value::Int(5)
    ));
    // json_pretty aliases jsonb_pretty.
    let pretty = text(&first(&mut e, "SELECT json_pretty('{\"a\": 1}')"));
    assert!(pretty.contains('\n'), "expected pretty output: {pretty}");
}
