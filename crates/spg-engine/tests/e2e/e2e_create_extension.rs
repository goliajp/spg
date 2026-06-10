//! v7.9.15 — F3: CREATE EXTENSION ... as no-op.

use spg_engine::{Engine, QueryResult};

fn ok(eng: &mut Engine, sql: &str) {
    let r = eng
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
    assert!(
        matches!(r, QueryResult::CommandOk { affected: 0, .. }),
        "{sql:?}"
    );
}

#[test]
fn create_extension_simple() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE EXTENSION vector");
}

#[test]
fn create_extension_if_not_exists() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE EXTENSION IF NOT EXISTS vector");
}

#[test]
fn create_extension_with_schema() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE EXTENSION pgcrypto WITH SCHEMA public");
}

#[test]
fn create_extension_with_cascade() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE EXTENSION IF NOT EXISTS hstore CASCADE");
}

#[test]
fn create_extension_does_not_modify_catalog() {
    // No-op should not bump the WAL — `modified_catalog: false`
    // signals to the snapshot driver that nothing changed.
    let mut eng = Engine::new();
    let r = eng.execute("CREATE EXTENSION vector").unwrap();
    match r {
        QueryResult::CommandOk {
            modified_catalog, ..
        } => {
            assert!(!modified_catalog);
        }
        _ => panic!("expected CommandOk"),
    }
}

#[test]
fn dual_target_dump_friendly() {
    // PG dumps prepend `CREATE EXTENSION IF NOT EXISTS vector;`.
    // SPG should accept it so the same SQL file runs against
    // both backends.
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE EXTENSION IF NOT EXISTS vector");
    ok(
        &mut eng,
        "CREATE TABLE docs (id INT NOT NULL, emb VECTOR(4) NOT NULL)",
    );
}
