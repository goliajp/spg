//! v7.37.17 (17.6 siblings) — session/context probes:
//! getdatabaseencoding + current_schemas + pg_trigger_depth +
//! txid_current_if_assigned + pg_jit_available + event-trigger
//! readers.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn getdatabaseencoding_utf8() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT getdatabaseencoding()") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "UTF8"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn current_schemas_search_path() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT current_schemas(false)") {
        spg_storage::Value::TextArray(items) => {
            assert_eq!(items, vec![Some("public".to_string())]);
        }
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT current_schemas(true)") {
        spg_storage::Value::TextArray(items) => {
            assert_eq!(
                items,
                vec![
                    Some("pg_catalog".to_string()),
                    Some("public".to_string())
                ]
            );
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn trigger_depth_and_jit() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_trigger_depth()"),
        spg_storage::Value::Int(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_jit_available()"),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn txid_current_if_assigned_is_null_without_an_id() {
    // v7.38 (T24) — PG returns NULL when the current transaction has not been
    // assigned an id, which is the case for a read-only autocommit statement.
    // This used to return the constant-1 stub.
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT txid_current_if_assigned()"),
        spg_storage::Value::Null
    ));
}

#[test]
fn event_trigger_readers_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_listening_channels()",
        "pg_event_trigger_ddl_commands()",
        "pg_event_trigger_dropped_objects()",
        "pg_event_trigger_table_rewrite_oid()",
        "pg_event_trigger_table_rewrite_reason()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
