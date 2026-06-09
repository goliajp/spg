//! v7.17.0 Phase 3.9 — jsonb_path_query family.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn text_array(v: &Value) -> Vec<Option<String>> {
    match v {
        Value::TextArray(a) => a.clone(),
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn path_query_root() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"k":1}'::JSONB, '$')"#).unwrap(),
    );
    assert_eq!(text_array(&r[0][0]), vec![Some(r#"{"k":1}"#.into())]);
}

#[test]
fn path_query_field() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"name":"alice","age":30}'::JSONB, '$.name')"#).unwrap(),
    );
    assert_eq!(text_array(&r[0][0]), vec![Some(r#""alice""#.into())]);
}

#[test]
fn path_query_nested_field() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"user":{"name":"bob"}}'::JSONB, '$.user.name')"#).unwrap(),
    );
    assert_eq!(text_array(&r[0][0]), vec![Some(r#""bob""#.into())]);
}

#[test]
fn path_query_array_index() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"items":[10,20,30]}'::JSONB, '$.items[1]')"#).unwrap(),
    );
    assert_eq!(text_array(&r[0][0]), vec![Some("20".into())]);
}

#[test]
fn path_query_wildcard() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"items":[1,2,3]}'::JSONB, '$.items[*]')"#).unwrap(),
    );
    assert_eq!(
        text_array(&r[0][0]),
        vec![Some("1".into()), Some("2".into()), Some("3".into())]
    );
}

#[test]
fn path_query_wildcard_with_field_after() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            r#"SELECT jsonb_path_query('{"users":[{"name":"a"},{"name":"b"}]}'::JSONB, '$.users[*].name')"#,
        )
        .unwrap(),
    );
    assert_eq!(
        text_array(&r[0][0]),
        vec![Some(r#""a""#.into()), Some(r#""b""#.into())]
    );
}

#[test]
fn path_query_no_match_empty_array() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query('{"k":1}'::JSONB, '$.missing')"#).unwrap(),
    );
    let a = text_array(&r[0][0]);
    assert!(a.is_empty());
}

#[test]
fn path_query_null_doc_propagates() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query(NULL::JSONB, '$.k')"#).unwrap(),
    );
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn path_query_first_returns_one() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query_first('{"items":[10,20]}'::JSONB, '$.items[*]')"#)
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::Json("10".into()));
}

#[test]
fn path_query_first_no_match_null() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(r#"SELECT jsonb_path_query_first('{"k":1}'::JSONB, '$.missing')"#).unwrap(),
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
    assert_eq!(r[0][0], Value::Json("[10,20,30]".into()));
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
