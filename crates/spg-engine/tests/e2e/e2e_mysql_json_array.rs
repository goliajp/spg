//! v7.37.17 (17.6 siblings) — MySQL json_array_append /
//! json_array_insert / json_contains.

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
fn array_append_vectors() {
    let mut e = Engine::new();
    // MySQL doc vectors on '["a", ["b", "c"], "d"]'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_array_append('[\"a\", [\"b\", \"c\"], \"d\"]', '$[1]', 1)"
        )),
        "[\"a\",[\"b\",\"c\",1],\"d\"]"
    );
    // Non-array target wraps as [old, new].
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_array_append('[\"a\", [\"b\", \"c\"], \"d\"]', '$[0]', 2)"
        )),
        "[[\"a\",2],[\"b\",\"c\"],\"d\"]"
    );
}

#[test]
fn array_insert_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_ARRAY_INSERT('["a", {"b": [1, 2]},
    // [3, 4]]', '$[1]', 'x') → ["a", "x", {"b": [1, 2]}, [3, 4]].
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_array_insert('[\"a\", {\"b\": [1, 2]}, [3, 4]]', '$[1]', 'x')"
        )),
        "[\"a\",\"x\",{\"b\":[1,2]},[3,4]]"
    );
    // Past-the-end appends.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_array_insert('[1, 2]', '$[100]', 3)"
        )),
        "[1,2,3]"
    );
    // Path not ending in [N] errors.
    let err = e
        .execute("SELECT json_array_insert('{\"a\": []}', '$.a', 1)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("array index"), "unexpected error: {msg}");
}

#[test]
fn json_contains_vectors() {
    let mut e = Engine::new();
    // MySQL doc vectors on '{"a": 1, "b": 2, "c": {"d": 4}}'.
    let doc = "'{\"a\": 1, \"b\": 2, \"c\": {\"d\": 4}}'";
    assert!(matches!(
        first(&mut e, &format!("SELECT json_contains({doc}, '1', '$.a')")),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, &format!("SELECT json_contains({doc}, '1', '$.b')")),
        spg_storage::Value::Bool(false)
    ));
    assert!(matches!(
        first(
            &mut e,
            &format!("SELECT json_contains({doc}, '{{\"d\": 4}}', '$.c')")
        ),
        spg_storage::Value::Bool(true)
    ));
    // Array containment: every candidate element must be contained.
    assert!(matches!(
        first(&mut e, "SELECT json_contains('[1, 2, 3]', '[1, 3]')"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_contains('[1, 2, 3]', '[1, 9]')"),
        spg_storage::Value::Bool(false)
    ));
    // Scalar in array.
    assert!(matches!(
        first(&mut e, "SELECT json_contains('[1, 2]', '2')"),
        spg_storage::Value::Bool(true)
    ));
}
