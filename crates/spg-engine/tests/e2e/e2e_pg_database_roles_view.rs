//! v7.17.0 Phase 3.P0-55 — pg_catalog.pg_database / pg_roles / pg_user views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_database_lists_the_database_current_database_names() {
    // v7.39 (round 474) — this asked for `postgres`, which was a hardcoded
    // literal in the synth. `current_database()` has always answered `spg`,
    // so the row named a database the rest of the engine did not know and a
    // client joining the two found nothing. The row now comes from the same
    // place `current_database()` reads.
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT datname FROM pg_catalog.pg_database WHERE datname = current_database()",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("spg"));
}

#[test]
fn pg_roles_includes_postgres_superuser() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT rolname, rolsuper FROM pg_catalog.pg_roles WHERE rolname = 'postgres'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("postgres"));
    assert_eq!(r[0][1], Value::Bool(true));
}

#[test]
fn pg_user_alias_returns_same_shape() {
    // v7.39 (round 542) — this asked pg_user for `rolname`, which is
    // pg_roles' column. PG's pg_user is a different view over the same
    // roles, and the name it publishes is `usename`; the assertion was
    // pinning SPG's divergence in place.
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT usename FROM pg_catalog.pg_user WHERE usename = 'postgres'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    // And the old spelling is gone, as it is in PG.
    assert!(
        e.execute("SELECT rolname FROM pg_user").is_err(),
        "pg_user must not answer pg_roles' column name"
    );
}
