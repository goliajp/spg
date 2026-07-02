//! v7.37.17 (17.6 siblings) — MySQL json_merge_patch (RFC 7396) /
//! json_merge_preserve / json_overlaps.

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
fn merge_patch_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_MERGE_PATCH('{"a": 1, "b": 2}',
    // '{"a": 3, "c": 4}') → {"a": 3, "b": 2, "c": 4}.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_merge_patch('{\"a\": 1, \"b\": 2}', '{\"a\": 3, \"c\": 4}')"
        )),
        "{\"a\":3,\"b\":2,\"c\":4}"
    );
    // null removes the key (RFC 7396).
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_merge_patch('{\"a\": 1, \"b\": 2}', '{\"b\": null}')"
        )),
        "{\"a\":1}"
    );
    // Non-object patch replaces wholesale.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_merge_patch('{\"a\": 1}', '[1, 2]')"
        )),
        "[1,2]"
    );
}

#[test]
fn merge_preserve_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: JSON_MERGE_PRESERVE('[1, 2]', '[true, false]')
    // → [1, 2, true, false].
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_merge_preserve('[1, 2]', '[true, false]')"
        )),
        "[1,2,true,false]"
    );
    // MySQL doc vector: duplicate keys combine into arrays.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT json_merge_preserve('{\"name\": \"x\"}', '{\"name\": \"y\"}')"
        )),
        "{\"name\":[\"x\",\"y\"]}"
    );
    // Scalars combine into an array; deprecated json_merge alias.
    assert_eq!(
        text(&first(&mut e, "SELECT json_merge('1', '2')")),
        "[1,2]"
    );
}

#[test]
fn overlaps_vectors() {
    let mut e = Engine::new();
    // MySQL doc vectors.
    assert!(matches!(
        first(&mut e, "SELECT json_overlaps('[1, 3, 5, 7]', '[2, 5, 7]')"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT json_overlaps('[1, 3, 5, 7]', '[2, 6, 8]')"),
        spg_storage::Value::Bool(false)
    ));
    // Objects share a key-value pair.
    assert!(matches!(
        first(
            &mut e,
            "SELECT json_overlaps('{\"a\": 1, \"b\": 10}', '{\"c\": 99, \"b\": 10}')"
        ),
        spg_storage::Value::Bool(true)
    ));
    // Scalar vs array membership.
    assert!(matches!(
        first(&mut e, "SELECT json_overlaps('[4, 5]', '5')"),
        spg_storage::Value::Bool(true)
    ));
}
