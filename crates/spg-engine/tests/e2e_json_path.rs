//! v6.4.5 — JSON path operators `#>`, `#>>`, `@>`.
//!
//! `->` and `->>` shipped in v4.14. v6.4.5 adds the path-walk
//! variants and the containment predicate.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_value(eng: &mut Engine, sql: &str) -> Value {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().next().unwrap().values[0].clone(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn hash_arrow_walks_into_object() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":{"b":42}}' #> '{a,b}'"#,
    );
    assert_eq!(v, Value::Json("42".to_string()));
}

#[test]
fn hash_arrow_text_returns_unquoted_string() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":{"b":"hi"}}' #>> '{a,b}'"#,
    );
    assert_eq!(v, Value::Text("hi".to_string()));
}

#[test]
fn hash_arrow_walks_into_array_index() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"items":[10,20,30]}' #> '{items,1}'"#,
    );
    assert_eq!(v, Value::Json("20".to_string()));
}

#[test]
fn hash_arrow_missing_key_returns_null() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":1}' #> '{b}'"#,
    );
    assert_eq!(v, Value::Null);
}

#[test]
fn contains_top_level_subset() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":1,"b":2,"c":3}' @> '{"a":1}'"#,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn contains_disjoint_keys_false() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":1}' @> '{"b":1}'"#,
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn contains_nested_object() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '{"a":{"b":1,"c":2}}' @> '{"a":{"b":1}}'"#,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn contains_array_subset() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT '[1,2,3,4]' @> '[2,4]'"#,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn null_propagates_through_path_ops() {
    let mut eng = Engine::new();
    let v = one_value(
        &mut eng,
        r#"SELECT NULL #> '{a}'"#,
    );
    assert_eq!(v, Value::Null);
}
