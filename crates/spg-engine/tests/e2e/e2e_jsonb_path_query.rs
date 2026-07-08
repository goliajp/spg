//! v7.17.0 Phase 3.9 — jsonb_path_query family.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

/// v7.38 (read01, T15) — jsonb_path_query is set-returning: collect the first
/// column of every emitted row as text. Oracle: live PG 18.4.
fn col0_texts(r: QueryResult) -> Vec<Option<String>> {
    rows(r)
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => Some(s.to_string()),
            Value::Null => None,
            other => panic!("expected Text/Null, got {other:?}"),
        })
        .collect()
}

#[test]
fn path_query_root() {
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_path_query('{"k":1}'::JSONB, '$')"#).unwrap();
    assert_eq!(col0_texts(r), vec![Some(r#"{"k": 1}"#.into())]);
}

#[test]
fn path_query_field() {
    let mut e = Engine::new();
    let r = e
        .execute(r#"SELECT jsonb_path_query('{"name":"alice","age":30}'::JSONB, '$.name')"#)
        .unwrap();
    assert_eq!(col0_texts(r), vec![Some(r#""alice""#.into())]);
}

#[test]
fn path_query_nested_field() {
    let mut e = Engine::new();
    let r = e
        .execute(r#"SELECT jsonb_path_query('{"user":{"name":"bob"}}'::JSONB, '$.user.name')"#)
        .unwrap();
    assert_eq!(col0_texts(r), vec![Some(r#""bob""#.into())]);
}

#[test]
fn path_query_array_index() {
    let mut e = Engine::new();
    let r = e
        .execute(r#"SELECT jsonb_path_query('{"items":[10,20,30]}'::JSONB, '$.items[1]')"#)
        .unwrap();
    assert_eq!(col0_texts(r), vec![Some("20".into())]);
}

#[test]
fn path_query_wildcard() {
    let mut e = Engine::new();
    let r = e
        .execute(r#"SELECT jsonb_path_query('{"items":[1,2,3]}'::JSONB, '$.items[*]')"#)
        .unwrap();
    assert_eq!(
        col0_texts(r),
        vec![Some("1".into()), Some("2".into()), Some("3".into())]
    );
}

#[test]
fn path_query_wildcard_with_field_after() {
    let mut e = Engine::new();
    let r = e
        .execute(
            r#"SELECT jsonb_path_query('{"users":[{"name":"a"},{"name":"b"}]}'::JSONB, '$.users[*].name')"#,
        )
        .unwrap();
    assert_eq!(
        col0_texts(r),
        vec![Some(r#""a""#.into()), Some(r#""b""#.into())]
    );
}

#[test]
fn path_query_no_match_empty() {
    // No match → zero rows (PG: the SRF emits nothing), not one row of `[]`.
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_path_query('{"k":1}'::JSONB, '$.missing')"#).unwrap();
    assert!(col0_texts(r).is_empty());
}

#[test]
fn path_query_null_doc_emits_no_rows() {
    // A NULL document yields zero rows (PG), not one NULL row.
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_path_query(NULL::JSONB, '$.k')"#).unwrap();
    assert!(rows(r).is_empty());
}

#[test]
fn path_query_first_returns_one() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query_first('{"items":[10,20]}'::JSONB, '$.items[*]')"#)
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::json("10"));
}

#[test]
fn path_query_first_no_match_null() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query_first('{"k":1}'::JSONB, '$.missing')"#)
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn path_query_array_returns_wrapped() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query_array('{"items":[10,20,30]}'::JSONB, '$.items[*]')"#)
            .unwrap(),
    );
    // jsonb output is canonical: `, ` after each element (live PG18.4
    // renders `[10, 20, 30]`, matching jsonb_agg / jsonb_build_array).
    assert_eq!(r[0][0], Value::json("[10, 20, 30]"));
}

#[test]
fn path_query_array_canonicalizes_output() {
    let mut e = Engine::new();
    // Nested objects and number normalization ride the canonical
    // renderer too (verified vs live PG18.4):
    //   $.a[*] over [{"x":1},{"x":2}] → [{"x": 1}, {"x": 2}]
    //   $[*]   over [1.10, 2e2, 3]    → [1.10, 200, 3]
    //   no match                      → []
    let case = |e: &mut Engine, doc: &str, path: &str| -> Value<'static> {
        rows(
            e.execute(&format!(
                "SELECT jsonb_path_query_array('{doc}'::JSONB, '{path}')"
            ))
            .unwrap(),
        )[0][0]
            .clone()
    };
    assert_eq!(
        case(&mut e, r#"{"a":[{"x":1},{"x":2}]}"#, "$.a[*]"),
        Value::json(r#"[{"x": 1}, {"x": 2}]"#)
    );
    assert_eq!(
        case(&mut e, "[1.10, 2e2, 3]", "$[*]"),
        Value::json("[1.10, 200, 3]")
    );
    assert_eq!(case(&mut e, r#"{"a":1}"#, "$.nope"), Value::json("[]"));
}

#[test]
fn path_query_invalid_path_errors() {
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_path_query('{}'::JSONB, 'no_dollar_prefix')"#);
    assert!(r.is_err());
}

#[test]
fn path_query_unsupported_filter_errors() {
    let mut e = Engine::new();
    let r = e.execute(r#"SELECT jsonb_path_query('[]'::JSONB, '$[?(@.k > 1)]')"#);
    assert!(r.is_err());
}
