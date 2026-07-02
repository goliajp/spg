//! v7.37.17 (17.6 siblings) — MySQL JSON path machinery:
//! json_extract + json_contains_path.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn json_extract_single_path() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_EXTRACT('[10, 20, [30, 40]]', '$[1]') → 20.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_extract('[10, 20, [30, 40]]', '$[1]')"
        )),
        "20"
    );
    // Nested object + index.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_extract('{\"a\": {\"b\": [1, 2]}}', '$.a.b[1]')"
        )),
        "2"
    );
    // Quoted key.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_extract('{\"a b\": 5}', '$.\"a b\"')"
        )),
        "5"
    );
    // Miss → NULL.
    assert!(matches!(
        first(&mut e, "SELECT json_extract('{\"a\": 1}', '$.zzz')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn json_extract_multiple_paths() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_EXTRACT('[10, 20, [30, 40]]', '$[1]',
    // '$[0]') → [20, 10].
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_extract('[10, 20, [30, 40]]', '$[1]', '$[0]')"
        )),
        "[20, 10]"
    );
}

#[test]
fn scalar_autowrap_at_index_zero() {
    let mut e = Engine::new();
    // MySQL: a scalar behaves as a one-element array for [0].
    assert_eq!(
        text(&first(&mut e, "SELECT json_extract('{\"a\": 5}', '$.a[0]')")),
        "5"
    );
}

#[test]
fn json_contains_path_one_and_all() {
    let mut e = Engine::new();
    // MySQL doc vectors on '{"a": 1, "b": 2, "c": {"d": 4}}'.
    let doc = "'{\"a\": 1, \"b\": 2, \"c\": {\"d\": 4}}'";
    assert!(matches!(
        first(
            &mut e,
            &format!("SELECT json_contains_path({doc}, 'one', '$.a', '$.e')")
        ),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(
            &mut e,
            &format!("SELECT json_contains_path({doc}, 'all', '$.a', '$.e')")
        ),
        spg_storage::Value::Bool(false)
    ));
    assert!(matches!(
        first(
            &mut e,
            &format!("SELECT json_contains_path({doc}, 'one', '$.c.d')")
        ),
        spg_storage::Value::Bool(true)
    ));
}

#[test]
fn wildcard_errors_honestly() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT json_extract('[1, 2]', '$[*]')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("wildcards are not supported"),
        "unexpected error: {msg}"
    );
}
