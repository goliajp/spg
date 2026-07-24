//! v7.37.17 (17.6 siblings) — MySQL JSON mutation family:
//! json_set / json_insert / json_replace / json_remove on the
//! '$.x' path machinery.

use spg_engine::{Engine, QueryResult};

fn first_text(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn json_set_replaces_and_creates() {
    let mut e = Engine::new();
    // MySQL doc shape on '{"a": 1, "b": [2, 3]}'.
    assert_eq!(
        first_text(
            &mut e,
            "SELECT json_set('{\"a\": 1, \"b\": [2, 3]}', '$.a', 10, '$.c', '[true, false]')"
        ),
        "{\"a\": 10, \"b\": [2, 3], \"c\": \"[true, false]\"}"
    );
    // Array index past the end appends.
    assert_eq!(
        first_text(&mut e, "SELECT json_set('[1, 2]', '$[5]', 9)"),
        "[1, 2, 9]"
    );
}

#[test]
fn json_insert_creates_only() {
    let mut e = Engine::new();
    // Existing key untouched, new key added.
    assert_eq!(
        first_text(
            &mut e,
            "SELECT json_insert('{\"a\": 1}', '$.a', 99, '$.b', 2)"
        ),
        "{\"a\": 1, \"b\": 2}"
    );
}

#[test]
fn json_replace_replaces_only() {
    let mut e = Engine::new();
    // Existing key replaced, missing key NOT created.
    assert_eq!(
        first_text(
            &mut e,
            "SELECT json_replace('{\"a\": 1}', '$.a', 99, '$.b', 2)"
        ),
        "{\"a\": 99}"
    );
}

#[test]
fn json_remove_keys_and_elements() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_REMOVE('[0, 1, 2, [3, 4]]', '$[0]',
    // '$[2]') → [1, 2] (paths evaluate left to right on the
    // already-modified document).
    assert_eq!(
        first_text(
            &mut e,
            "SELECT json_remove('[0, 1, 2, [3, 4]]', '$[0]', '$[2]')"
        ),
        "[1, 2]"
    );
    assert_eq!(
        first_text(&mut e, "SELECT json_remove('{\"a\": 1, \"b\": 2}', '$.b')"),
        "{\"a\": 1}"
    );
    // Root removal errors like MySQL.
    let err = e
        .execute("SELECT json_remove('{\"a\": 1}', '$')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("not allowed"), "unexpected error: {msg}");
}

#[test]
fn pg_jsonb_set_spelling_still_works() {
    let mut e = Engine::new();
    // The PG text-array path form must keep routing to json::set.
    let got = first_text(&mut e, "SELECT jsonb_set('{\"a\": 1}', '{a}', '2')");
    assert!(got.contains("\"a\""), "unexpected: {got}");
    assert!(got.contains('2'), "unexpected: {got}");
}
