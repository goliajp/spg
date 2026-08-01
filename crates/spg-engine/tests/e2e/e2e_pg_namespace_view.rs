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
fn pg_namespace_lists_the_schemas_that_exist() {
    let mut e = Engine::new();
    // v7.39 (round 661) — was `COUNT(*) == 3`. A bare count says nothing
    // about WHICH schemas are there, and it went red the moment
    // `spg_catalog` was added to hold the 86 functions SPG answers that
    // PG18 does not have. Assert the contents instead.
    let r = rows(
        e.execute("SELECT nspname FROM pg_catalog.pg_namespace ORDER BY oid")
            .unwrap(),
    );
    let got: Vec<String> = r
        .iter()
        .map(|row| spg_engine::eval::value_to_text(&row[0]))
        .collect();
    assert_eq!(
        got,
        vec!["pg_catalog", "public", "information_schema", "spg_catalog"]
    );
}
