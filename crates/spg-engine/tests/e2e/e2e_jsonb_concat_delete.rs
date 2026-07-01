//! v7.37.17 (17.6 siblings) — jsonb_concat + jsonb_delete.

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
fn concat_objects_right_wins() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_concat('{"a": 1, "b": 2}', '{"b": 3, "c": 4}')"#
        )),
        r#"{"a":1,"b":3,"c":4}"#
    );
}

#[test]
fn concat_arrays_appends() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_concat('[1, 2]', '[3, 4]')")),
        "[1,2,3,4]"
    );
    // Array + scalar appends.
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_concat('[1, 2]', '3')")),
        "[1,2,3]"
    );
    // Scalar + scalar makes a 2-array.
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_concat('1', '2')")),
        "[1,2]"
    );
}

#[test]
fn delete_object_key() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete('{"a": 1, "b": 2}', 'a')"#
        )),
        r#"{"b":2}"#
    );
    // Missing key: unchanged.
    assert_eq!(
        json(&first(
            &mut e,
            r#"SELECT jsonb_delete('{"a": 1}', 'zzz')"#
        )),
        r#"{"a":1}"#
    );
}

#[test]
fn delete_array_index() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_delete('[10, 20, 30]', 1)")),
        "[10,30]"
    );
    // Negative index counts from the end.
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_delete('[10, 20, 30]', -1)")),
        "[10,20]"
    );
}

#[test]
fn concat_delete_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_concat(NULL::text, '[1]')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, r#"SELECT jsonb_delete('{"a":1}', NULL::text)"#),
        spg_storage::Value::Null
    ));
}
