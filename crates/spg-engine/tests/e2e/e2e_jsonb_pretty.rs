//! v7.37.17 (17.6 siblings) — jsonb_pretty.

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

#[test]
fn jsonb_pretty_object() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT jsonb_pretty('{\"a\":1,\"b\":2}'::jsonb)");
    match v {
        spg_storage::Value::Text(s) => {
            // 2-space indent, one member per line, no trailing comma.
            assert_eq!(s.as_ref(), "{\n    \"a\": 1,\n    \"b\": 2\n}");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_pretty_array() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT jsonb_pretty('[1,2,3]'::jsonb)");
    match v {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "[\n    1,\n    2,\n    3\n]"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_pretty_empty_containers() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT jsonb_pretty('{}'::jsonb)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "{}"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT jsonb_pretty('[]'::jsonb)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "[]"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_pretty_nested() {
    let mut e = Engine::new();
    let v = first(
        &mut e,
        "SELECT jsonb_pretty('{\"a\":{\"b\":[1,2]}}'::jsonb)",
    );
    match v {
        spg_storage::Value::Text(s) => {
            assert_eq!(
                s.as_ref(),
                "{\n    \"a\": {\n        \"b\": [\n            1,\n            2\n        ]\n    }\n}"
            );
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_pretty_null_and_scalar() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_pretty(NULL::jsonb)"),
        spg_storage::Value::Null
    ));
    // Scalar top-level (PG accepts).
    match first(&mut e, "SELECT jsonb_pretty('42'::jsonb)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "42"),
        other => panic!("got {other:?}"),
    }
}
