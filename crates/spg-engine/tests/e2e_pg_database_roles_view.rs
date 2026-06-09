//! v7.17.0 Phase 3.P0-55 — pg_catalog.pg_database / pg_roles / pg_user views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_database_lists_postgres() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT datname FROM pg_catalog.pg_database WHERE datname = 'postgres'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("postgres".into()));
}

#[test]
fn pg_roles_includes_postgres_superuser() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT rolname, rolsuper FROM pg_catalog.pg_roles WHERE rolname = 'postgres'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("postgres".into()));
    assert_eq!(r[0][1], Value::Bool(true));
}

#[test]
fn pg_user_alias_returns_same_shape() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT rolname FROM pg_catalog.pg_user WHERE rolname = 'postgres'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
}
