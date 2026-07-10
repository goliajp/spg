//! v7.37.17 (17.6 siblings) — json_object(text[]) / jsonb_object.

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
fn json_object_flat_array() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            "SELECT json_object(ARRAY['a', '1', 'b', '2'])"
        )),
        r#"{"a" : "1", "b" : "2"}"#
    );
    // Empty array → empty object.
    assert_eq!(
        json(&first(&mut e, "SELECT json_object(ARRAY[]::text[])")),
        "{}"
    );
}

#[test]
fn json_object_two_arrays() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(
            &mut e,
            "SELECT json_object(ARRAY['x', 'y'], ARRAY['10', '20'])"
        )),
        r#"{"x" : "10", "y" : "20"}"#
    );
}

#[test]
fn json_object_escapes_quotes() {
    let mut e = Engine::new();
    let v = json(&first(
        &mut e,
        r#"SELECT json_object(ARRAY['k', 'has "quote"'])"#,
    ));
    assert_eq!(v, r#"{"k" : "has \"quote\""}"#);
}

#[test]
fn json_object_odd_length_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT json_object(ARRAY['a', '1', 'b'])").is_err());
    // Length mismatch on 2-array form.
    assert!(e
        .execute("SELECT json_object(ARRAY['a'], ARRAY['1', '2'])")
        .is_err());
}

#[test]
fn json_object_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT json_object(NULL::text[])"),
        spg_storage::Value::Null
    ));
}

#[test]
fn jsonb_object_alias() {
    let mut e = Engine::new();
    assert_eq!(
        json(&first(&mut e, "SELECT jsonb_object(ARRAY['a', '1'])")),
        r#"{"a": "1"}"#
    );
}
