//! v7.37.17 (17.6 siblings) — SQL:2016 / PG 16 IS [NOT] JSON
//! [VALUE|OBJECT|ARRAY|SCALAR] predicate.

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

fn b(v: spg_storage::Value<'static>) -> bool {
    match v {
        spg_storage::Value::Bool(x) => x,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn is_json_kinds() {
    let mut e = Engine::new();
    assert!(b(first(&mut e, "SELECT '{\"a\": 1}' IS JSON")));
    assert!(b(first(&mut e, "SELECT '{\"a\": 1}' IS JSON OBJECT")));
    assert!(!b(first(&mut e, "SELECT '{\"a\": 1}' IS JSON ARRAY")));
    assert!(b(first(&mut e, "SELECT '[1, 2]' IS JSON ARRAY")));
    assert!(b(first(&mut e, "SELECT '123' IS JSON SCALAR")));
    assert!(!b(first(&mut e, "SELECT '[1]' IS JSON SCALAR")));
    // Invalid JSON is false, never an error.
    assert!(!b(first(&mut e, "SELECT 'not json' IS JSON")));
}

#[test]
fn is_not_json_negates() {
    let mut e = Engine::new();
    assert!(b(first(&mut e, "SELECT 'not json' IS NOT JSON")));
    assert!(!b(first(&mut e, "SELECT '[]' IS NOT JSON")));
    assert!(b(first(&mut e, "SELECT '{\"a\": 1}' IS NOT JSON ARRAY")));
}

#[test]
fn composes_in_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE docs (body TEXT)").unwrap();
    e.execute("INSERT INTO docs VALUES ('{\"ok\": true}'), ('oops'), ('[1]')")
        .unwrap();
    let r = e
        .execute("SELECT COUNT(*) FROM docs WHERE body IS JSON")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert!(matches!(
        rows[0].values[0],
        spg_storage::Value::Int(2) | spg_storage::Value::BigInt(2)
    ));
}
