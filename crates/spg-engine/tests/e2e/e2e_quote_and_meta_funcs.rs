//! v7.37.17 (17.6 siblings) — quote_ident / quote_literal /
//! quote_nullable / format_type / obj_description / to_reg* /
//! pg_client_encoding / pg_is_in_recovery.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn quote_ident_wraps_in_double_quotes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT quote_ident('foo')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "\"foo\""),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_ident('foo\"bar')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "\"foo\"\"bar\""),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn quote_literal_wraps_in_single_quotes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT quote_literal('bar')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'bar'"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_literal('it''s')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'it''s'"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn quote_nullable_returns_null_text_for_null() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT quote_nullable(NULL)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "NULL"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_nullable('x')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'x'"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn to_regclass_returns_null_for_now() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_regclass('some_table')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT to_regtype('int')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn recovery_and_encoding_probes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_is_in_recovery()") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_client_encoding()") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "UTF8"),
        other => panic!("got {other:?}"),
    }
}
