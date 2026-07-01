//! v7.37.17 (17.6 siblings) — jsonb_object_keys returns TextArray
//! of top-level keys (SPG scalar surface; PG has this as SRF).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn jsonb_object_keys_returns_keys_in_order() {
    let mut e = Engine::new();
    let v = first(
        &mut e,
        "SELECT jsonb_object_keys('{\"a\":1,\"b\":2,\"c\":3}'::jsonb)",
    );
    match &v {
        spg_storage::Value::TextArray(items) => {
            let s: Vec<_> = items.iter().map(|o| o.clone().unwrap()).collect();
            assert_eq!(s, ["a", "b", "c"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_object_keys_empty_object_empty_array() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT jsonb_object_keys('{}'::jsonb)");
    match &v {
        spg_storage::Value::TextArray(items) => assert!(items.is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn jsonb_object_keys_errors_on_non_object() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT jsonb_object_keys('[1,2,3]'::jsonb)")
            .is_err()
    );
    assert!(e.execute("SELECT jsonb_object_keys('42'::jsonb)").is_err());
}

#[test]
fn jsonb_object_keys_null_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT jsonb_object_keys(NULL::jsonb)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn json_object_keys_synonym_works() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT json_object_keys('{\"x\":1}'::jsonb)");
    match &v {
        spg_storage::Value::TextArray(items) => {
            let s: Vec<_> = items.iter().map(|o| o.clone().unwrap()).collect();
            assert_eq!(s, ["x"]);
        }
        other => panic!("got {other:?}"),
    }
}
