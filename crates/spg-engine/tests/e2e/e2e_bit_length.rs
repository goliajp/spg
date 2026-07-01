//! v7.37.17 (17.6 sibling) — SQL:2003 BIT_LENGTH.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn bit_length_returns_octet_length_times_8() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT bit_length('abc')") {
        spg_storage::Value::Int(24) => {}
        other => panic!("expected 24, got {other:?}"),
    }
    match first(&mut e, "SELECT bit_length('')") {
        spg_storage::Value::Int(0) => {}
        other => panic!("expected 0, got {other:?}"),
    }
    // UTF-8: 中 is 3 bytes = 24 bits.
    match first(&mut e, "SELECT bit_length('中')") {
        spg_storage::Value::Int(24) => {}
        other => panic!("expected 24, got {other:?}"),
    }
}

#[test]
fn bit_length_null_input_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT bit_length(NULL::text)"),
        spg_storage::Value::Null
    ));
}
