//! v7.37.17 (17.6 siblings) — jsonb_set_lax + json(b)_to_tsvector
//! + pg_collation_for.

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

fn text_or_json(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Json(s) => s.to_string(),
        other => panic!("expected Text/Json, got {other:?}"),
    }
}

#[test]
fn set_lax_non_null_behaves_like_set() {
    let mut e = Engine::new();
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_set_lax('{"a": 1}', '{a}', '2')"#
        )),
        r#"{"a": 2}"# // v7.38: jsonb_set_lax canonicalises like jsonb_set (live PG18.4)
    );
}

#[test]
fn set_lax_null_default_uses_json_null() {
    let mut e = Engine::new();
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_set_lax('{"a": 1}', '{a}', NULL)"#
        )),
        r#"{"a": null}"# // v7.38: spaced jsonb, live PG18.4
    );
}

#[test]
fn set_lax_treatments() {
    let mut e = Engine::new();
    // return_target — unchanged doc.
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_set_lax('{"a": 1}', '{a}', NULL, true, 'return_target')"#
        )),
        r#"{"a": 1}"#
    );
    // delete_key — the key is removed.
    assert_eq!(
        text_or_json(&first(
            &mut e,
            r#"SELECT jsonb_set_lax('{"a": 1, "b": 2}', '{a}', NULL, true, 'delete_key')"#
        )),
        r#"{"b": 2}"#
    );
    // raise_exception — errors.
    assert!(
        e.execute(r#"SELECT jsonb_set_lax('{"a": 1}', '{a}', NULL, true, 'raise_exception')"#)
            .is_err()
    );
    // Unknown treatment — errors.
    assert!(
        e.execute(r#"SELECT jsonb_set_lax('{"a": 1}', '{a}', NULL, true, 'bogus')"#)
            .is_err()
    );
}

#[test]
fn jsonb_to_tsvector_filters() {
    let mut e = Engine::new();
    // v7.39 (read01 round 43) — json(b)_to_tsvector now returns a real
    // stemmed, positioned tsvector. Byte-locked vs PG18 with the config
    // spelled explicitly ('english'); the 2-arg default-config form is a
    // recorded residual (SPG's FTS default resolves to 'simple', PG's to
    // 'english' — a global default-config gap, see note).
    // string filter — english drops the stopword 'the'.
    assert_eq!(
        spg_engine::eval::value_to_text(&first(
            &mut e,
            r#"SELECT jsonb_to_tsvector('english', '{"a":"The Fat","b":123}', '["string"]')"#
        )),
        "'fat':2"
    );
    // all — strings + numerics + booleans + keys; 'a' is an english
    // stopword (dropped), and values are joined with a one-position gap.
    assert_eq!(
        spg_engine::eval::value_to_text(&first(
            &mut e,
            r#"SELECT jsonb_to_tsvector('english', '{"a":"cat","b":123,"c":true}', '"all"')"#
        )),
        "'123':6 'b':4 'c':8 'cat':2 'true':10"
    );
    // numeric only.
    assert_eq!(
        spg_engine::eval::value_to_text(&first(
            &mut e,
            r#"SELECT json_to_tsvector('english', '{"a":"cat","b":123}', '["numeric"]')"#
        )),
        "'123':1"
    );
    // Unknown flag errors.
    assert!(
        e.execute(r#"SELECT jsonb_to_tsvector('{"a": 1}', '["bogus"]')"#)
            .is_err()
    );
}

#[test]
fn pg_collation_for_text_default() {
    let mut e = Engine::new();
    assert_eq!(
        text_or_json(&first(&mut e, "SELECT pg_collation_for('abc')")),
        "\"default\""
    );
    // Non-collatable type errors like PG.
    assert!(e.execute("SELECT pg_collation_for(42)").is_err());
}

#[test]
fn set_lax_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, r#"SELECT jsonb_set_lax(NULL::text, '{a}', '1')"#),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT jsonb_to_tsvector(NULL::text, '\"all\"')"),
        spg_storage::Value::Null
    ));
}
