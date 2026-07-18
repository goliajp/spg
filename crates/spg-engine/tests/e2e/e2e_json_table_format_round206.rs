//! v7.39 (read01 round 206) — JSON_TABLE Phase 1: FORMAT JSON /
//! WITH WRAPPER column mode. Byte-identical vs live PG18.4
//! (2026-07-18): a FORMAT JSON column returns the PG-canonical json
//! representation (spaced, strings quoted); WITH WRAPPER wraps the
//! whole match set in a json array (even a single scalar → `[5]`,
//! a multi-match path → `[10, 20]`).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Json(s) => s.to_string(),
            spg_storage::Value::Text(s) => s.to_string(),
            other => format!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn format_json_wrapper_array() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT t FROM json_table('[{\"t\":[1,2,3]}]', '$[*]' \
             COLUMNS (t TEXT FORMAT JSON PATH '$.t' WITH WRAPPER)) jt"
        ),
        "[[1, 2, 3]]"
    );
}

#[test]
fn format_json_no_wrapper() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT o FROM json_table('[{\"o\":{\"x\":1}}]', '$[*]' \
             COLUMNS (o JSONB FORMAT JSON PATH '$.o')) jt"
        ),
        "{\"x\": 1}"
    );
    // A json string stays quoted under FORMAT JSON.
    assert_eq!(
        one(
            &mut e,
            "SELECT s FROM json_table('[{\"s\":\"hi\"}]', '$[*]' \
             COLUMNS (s TEXT FORMAT JSON PATH '$.s')) jt"
        ),
        "\"hi\""
    );
}

#[test]
fn wrapper_wraps_single_scalar() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT v FROM json_table('[{\"v\":5}]', '$[*]' \
             COLUMNS (v TEXT FORMAT JSON PATH '$.v' WITH WRAPPER)) jt"
        ),
        "[5]"
    );
}

#[test]
fn wrapper_wraps_multi_match() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT a FROM json_table('[{\"a\":[10,20]}]', '$[*]' \
             COLUMNS (a TEXT FORMAT JSON PATH '$.a[*]' WITH WRAPPER)) jt"
        ),
        "[10, 20]"
    );
}
