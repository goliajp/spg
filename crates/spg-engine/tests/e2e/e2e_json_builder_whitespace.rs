//! v7.38 (read01, T-json-ws) — PG's json-builder whitespace. The `*_build_*`
//! constructors and json_object pretty-space their output; `to_json` /
//! `array_to_json` / `row_to_json` stay compact. json objects use ` : `
//! (spaces both sides); jsonb objects canonicalise to `: `. Every expected
//! value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Json(s) => s.to_string(),
            other => format!("{other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn json_build_array_uses_comma_space() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(&mut e, "SELECT json_build_array(1,'a',true)::text"),
        "[1, \"a\", true]"
    );
    assert_eq!(scalar(&mut e, "SELECT json_build_array()::text"), "[]");
    assert_eq!(scalar(&mut e, "SELECT json_build_array(1)::text"), "[1]");
}

#[test]
fn json_build_object_uses_space_colon_space() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(&mut e, "SELECT json_build_object('a',1,'b','x')::text"),
        "{\"a\" : 1, \"b\" : \"x\"}"
    );
    assert_eq!(scalar(&mut e, "SELECT json_build_object()::text"), "{}");
}

#[test]
fn jsonb_builders_canonicalise() {
    let mut e = Engine::new();
    // jsonb objects use `: ` (canonical), arrays use `, `.
    assert_eq!(
        scalar(&mut e, "SELECT jsonb_build_object('a',1,'b','x')::text"),
        "{\"a\": 1, \"b\": \"x\"}"
    );
    assert_eq!(
        scalar(&mut e, "SELECT jsonb_build_array(1,'a',true)::text"),
        "[1, \"a\", true]"
    );
}

#[test]
fn json_builders_nest() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT json_build_object('a', json_build_array(1,2))::text"
        ),
        "{\"a\" : [1, 2]}"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT json_build_array(json_build_object('k',1), 2)::text"
        ),
        "[{\"k\" : 1}, 2]"
    );
}

#[test]
fn json_object_from_array_uses_space_colon_space() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(&mut e, "SELECT json_object('{a,1,b,2}')::text"),
        "{\"a\" : \"1\", \"b\" : \"2\"}"
    );
    assert_eq!(
        scalar(&mut e, "SELECT jsonb_object('{a,1,b,2}')::text"),
        "{\"a\": \"1\", \"b\": \"2\"}"
    );
}

#[test]
fn to_json_and_row_to_json_stay_compact() {
    let mut e = Engine::new();
    // Serialisers (not builders) keep PG's compact form.
    assert_eq!(
        scalar(&mut e, "SELECT to_json(ARRAY[1,2,3])::text"),
        "[1,2,3]"
    );
    assert_eq!(
        scalar(&mut e, "SELECT array_to_json(ARRAY[1,2,3])::text"),
        "[1,2,3]"
    );
    assert_eq!(
        scalar(&mut e, "SELECT row_to_json(ROW(1,2))::text"),
        "{\"f1\":1,\"f2\":2}"
    );
}
