//! v7.39 (read01 utils/adt, round 38, jsonb deep water) — the ::jsonb
//! cast rejects invalid tokens (NaN/Infinity), jsonb_object_keys returns
//! canonical (sorted) key order, and the jsonb_path functions accept the
//! strict/lax mode prefix. Byte-locked vs PG18.

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

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn jsonb_cast_rejects_invalid_tokens() {
    let mut e = Engine::new();
    assert!(err_of(&mut e, "SELECT '{\"a\": NaN}'::jsonb")
        .contains("invalid input syntax for type json"));
    assert!(err_of(&mut e, "SELECT '{\"a\": Infinity}'::jsonb")
        .contains("invalid input syntax for type json"));
    // Valid jsonb still canonicalizes.
    assert_eq!(
        row_of(&mut e, "SELECT '  {  \"a\" : 1 }  '::jsonb"),
        vec!["{\"a\": 1}"]
    );
}

#[test]
fn jsonb_object_keys_canonical_order() {
    let mut e = Engine::new();
    // jsonb keys sort by (length, then bytes); json keeps insertion order.
    assert_eq!(
        col_of(&mut e, "SELECT jsonb_object_keys('{\"z\":1,\"a\":2,\"m\":3}')"),
        vec!["a", "m", "z"]
    );
    assert_eq!(
        col_of(&mut e, "SELECT json_object_keys('{\"z\":1,\"a\":2,\"m\":3}')"),
        vec!["z", "a", "m"]
    );
}

#[test]
fn jsonpath_strict_lax_prefix_parses() {
    let mut e = Engine::new();
    // The mode word is accepted (SPG evaluates lax semantics).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT jsonb_path_query_array('{\"a\":[1,2,3]}', 'strict $.a[*]'), \
             jsonb_path_query_array('{\"a\":[1,2,3]}', 'lax $.a[*]')"
        ),
        vec!["[1, 2, 3]", "[1, 2, 3]"]
    );
}
