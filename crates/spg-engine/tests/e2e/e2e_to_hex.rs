//! v7.37.17 (17.6 siblings) — PG to_hex(int|bigint).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn to_hex_int() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT to_hex(255)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "ff"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT to_hex(0)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "0"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT to_hex(65535)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "ffff"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn to_hex_bigint() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT to_hex(4294967296::bigint)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "100000000"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn to_hex_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_hex(NULL::int)"),
        spg_storage::Value::Null
    ));
}
