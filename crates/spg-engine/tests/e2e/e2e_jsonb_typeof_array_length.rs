//! v7.37.17 (17.6 siblings) — jsonb_typeof / json_typeof /
//! jsonb_array_length / json_array_length.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn jsonb_typeof_object() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT jsonb_typeof('{\"a\": 1}'::jsonb)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "object"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_typeof_all_variants() {
    let mut e = Engine::new();
    let cases = [
        ("'{}'", "object"),
        ("'[]'", "array"),
        ("'\"hello\"'", "string"),
        ("'42'", "number"),
        ("'-3.14'", "number"),
        ("'true'", "boolean"),
        ("'false'", "boolean"),
        ("'null'", "null"),
    ];
    for (input, expected) in cases {
        let sql = format!("SELECT jsonb_typeof({input}::jsonb)");
        match first(&mut e, &sql) {
            spg_storage::Value::Text(s) => {
                assert_eq!(s.as_ref(), expected, "jsonb_typeof({input})")
            }
            other => panic!("jsonb_typeof({input}): {other:?}"),
        }
    }
}

#[test]
fn jsonb_array_length_basic() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT jsonb_array_length('[1, 2, 3]'::jsonb)") {
        spg_storage::Value::Int(3) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT jsonb_array_length('[]'::jsonb)") {
        spg_storage::Value::Int(0) => {}
        other => panic!("got {other:?}"),
    }
    match first(
        &mut e,
        "SELECT jsonb_array_length('[\"a\", \"b\", \"c\", \"d\"]'::jsonb)",
    ) {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_array_length_errors_on_non_array() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT jsonb_array_length('{\"a\": 1}'::jsonb)")
            .is_err()
    );
    assert!(
        e.execute("SELECT jsonb_array_length('42'::jsonb)").is_err()
    );
}

#[test]
fn json_typeof_length_synonyms_work() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT json_typeof('{\"a\": 1}'::jsonb)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "object"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT json_array_length('[1, 2]'::jsonb)") {
        spg_storage::Value::Int(2) => {}
        other => panic!("got {other:?}"),
    }
}
