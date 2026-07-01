//! v7.37.17 (17.6 siblings) — PG 16+ pg_input_is_valid.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_bool(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn integer_valid_and_invalid() {
    let mut e = Engine::new();
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('123', 'integer')")));
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('-1', 'integer')")));
    assert!(!as_bool(&first(&mut e, "SELECT pg_input_is_valid('abc', 'integer')")));
    assert!(!as_bool(&first(&mut e, "SELECT pg_input_is_valid('99999999999999', 'integer')")));
    // Fits in bigint.
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('99999999999999', 'bigint')")));
}

#[test]
fn boolean_variants() {
    let mut e = Engine::new();
    for input in &["true", "false", "t", "f", "TRUE", "yes", "no", "1", "0"] {
        let sql = format!("SELECT pg_input_is_valid('{input}', 'boolean')");
        assert!(as_bool(&first(&mut e, &sql)), "{input} should be valid boolean");
    }
    assert!(!as_bool(&first(&mut e, "SELECT pg_input_is_valid('maybe', 'boolean')")));
}

#[test]
fn numeric_and_float() {
    let mut e = Engine::new();
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('3.14', 'numeric')")));
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('1e10', 'double precision')")));
    assert!(!as_bool(&first(&mut e, "SELECT pg_input_is_valid('abc', 'numeric')")));
}

#[test]
fn text_always_valid() {
    let mut e = Engine::new();
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('anything', 'text')")));
    assert!(as_bool(&first(&mut e, "SELECT pg_input_is_valid('', 'text')")));
}

#[test]
fn null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_input_is_valid(NULL::text, 'integer')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_input_is_valid('1', NULL::text)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn pg_input_error_info_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_input_error_info('1', 'integer')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_input_error_message('abc', 'integer')"),
        spg_storage::Value::Null
    ));
}
