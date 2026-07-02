//! v7.37.17 (17.6 siblings) — catalog function forms of -> / ->>:
//! json_object_field / json_array_element + _text + jsonb_ twins.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text_or_json(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn object_field_json_and_text() {
    let mut e = Engine::new();
    // -> keeps JSON form (string keeps quotes).
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT json_object_field('{"a":"x","b":2}', 'a')"#
        )),
        r#""x""#
    );
    // ->> unwraps to text.
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_object_field_text('{"a":"x","b":2}', 'a')"#
        )),
        "x"
    );
    // Missing key → NULL.
    assert!(matches!(
        first(
            &mut e,
            r#"SELECT json_object_field('{"a":1}', 'zzz')"#
        ),
        spg_storage::Value::Null
    ));
}

#[test]
fn array_element_json_and_text() {
    let mut e = Engine::new();
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT json_array_element('[10, "two", 30]', 1)"#
        )),
        r#""two""#
    );
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_array_element_text('[10, "two", 30]', 1)"#
        )),
        "two"
    );
    // Out of range → NULL.
    assert!(matches!(
        first(&mut e, r#"SELECT json_array_element('[1]', 5)"#),
        spg_storage::Value::Null
    ));
}

#[test]
fn field_fns_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        r#"json_object_field(NULL::text, 'a')"#,
        r#"json_object_field('{"a":1}', NULL::text)"#,
        r#"json_array_element(NULL::text, 0)"#,
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
