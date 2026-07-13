//! v7.39 (read01 utils/adt, json.c + jsonb.c part 1) — gaps found by
//! differential vs PG18: quoted non-finite numbers in to_json, the
//! timestamptz ISO-with-offset encoding, the strict/unique aggregate
//! variants, json_object_agg's distinctive spacing, row_to_json's
//! pretty flag, and SQL:2016 json_scalar/json_serialize.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn to_json_nonfinite_and_timestamptz() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_json('NaN'::float8), to_json('inf'::float8), to_json('-inf'::float8), \
             to_json('NaN'::numeric)"
        ),
        vec!["\"NaN\"", "\"Infinity\"", "\"-Infinity\"", "\"NaN\""]
    );
    // timestamptz gets the session-zone offset; plain timestamp doesn't.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_json(TIMESTAMPTZ '2024-03-09 14:05:06+00'), \
             to_json(TIMESTAMP '2024-03-09 14:05:06')"
        ),
        vec![
            "\"2024-03-09T14:05:06+00:00\"",
            "\"2024-03-09T14:05:06\""
        ]
    );
}

#[test]
fn strict_and_unique_aggregate_variants() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT json_agg_strict(x) FROM (VALUES (1),(NULL),(3)) v(x)"
        ),
        vec!["[1, 3]"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT json_object_agg_strict(k, v) FROM (VALUES ('a',1),('b',NULL)) t(k,v)"
        ),
        vec!["{ \"a\" : 1 }"]
    );
    let err = e
        .execute("SELECT json_object_agg_unique(k, v) FROM (VALUES ('a',1),('a',2)) t(k,v)")
        .unwrap_err();
    assert!(
        format!("{err}").contains("duplicate JSON object key value: \"a\""),
        "{err}"
    );
}

#[test]
fn json_object_agg_spacing_and_pretty_row() {
    let mut e = Engine::new();
    // PG's json_object_agg emits "{ \"k\" : v, ... }".
    assert_eq!(
        row_of(
            &mut e,
            "SELECT json_object_agg(k, v) FROM (VALUES ('a',1),('b',2)) t(k,v)"
        ),
        vec!["{ \"a\" : 1, \"b\" : 2 }"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT row_to_json(ROW(1,'a'), true)"),
        vec!["{\"f1\":1,\n \"f2\":\"a\"}"]
    );
}

#[test]
fn json_scalar_and_serialize() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT json_scalar(42), json_scalar('x'), json_serialize('{\"a\":1}')"
        ),
        vec!["42", "\"x\"", "{\"a\":1}"]
    );
}
