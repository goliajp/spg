//! v7.37.17 (17.6 siblings) — pg_column_size + pg_column_compression.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn pg_column_size_scalar_types() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_column_size(NULL)") {
        spg_storage::Value::Int(0) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_column_size(true)") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_column_size(42)") {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_column_size(42::bigint)") {
        spg_storage::Value::Int(8) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_column_size_text_includes_varlena_header() {
    let mut e = Engine::new();
    // 'abc' = 3 bytes text + 4-byte varlena header = 7.
    match first(&mut e, "SELECT pg_column_size('abc')") {
        spg_storage::Value::Int(7) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_column_size('')") {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_column_compression_returns_plain_or_null() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_column_compression('hello')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "plain"),
        other => panic!("got {other:?}"),
    }
    // Non-varlena types → NULL.
    match first(&mut e, "SELECT pg_column_compression(42)") {
        spg_storage::Value::Null => {}
        other => panic!("got {other:?}"),
    }
}
