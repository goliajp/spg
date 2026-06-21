//! v7.17.0 Phase 3.P0-57 — pg_catalog.pg_settings virtual view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_settings_exposes_server_version() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT setting FROM pg_catalog.pg_settings WHERE name = 'server_version'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    let s = match &r[0][0] {
        Value::Text(v) => v.clone(),
        _ => panic!(),
    };
    assert!(s.contains("spg"));
}

#[test]
fn pg_settings_has_standard_conforming_strings_on() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT setting FROM pg_catalog.pg_settings WHERE name = 'standard_conforming_strings'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("on"));
}

#[test]
fn pg_settings_lists_client_encoding() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT setting FROM pg_catalog.pg_settings WHERE name = 'client_encoding'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("UTF8"));
}

#[test]
fn pg_settings_session_set_value_overrides_default() {
    let mut e = Engine::new();
    e.execute("SET search_path TO custom_schema").unwrap();
    let r = rows(
        e.execute("SELECT setting FROM pg_catalog.pg_settings WHERE name = 'search_path'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("custom_schema"));
}
