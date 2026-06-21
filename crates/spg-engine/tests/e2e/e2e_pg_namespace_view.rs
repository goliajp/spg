//! v7.17.0 Phase 3.P0-52 — `pg_catalog.pg_namespace` virtual view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_namespace_lists_public() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(2200));
}

#[test]
fn pg_namespace_lists_pg_catalog() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'pg_catalog'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(11));
}

#[test]
fn pg_namespace_lists_information_schema() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'information_schema'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(13000));
}

#[test]
fn pg_namespace_has_three_rows() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM pg_catalog.pg_namespace")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(3));
}
