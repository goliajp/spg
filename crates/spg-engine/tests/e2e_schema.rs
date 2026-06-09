//! v7.17.0 Phase 1.6 — CREATE SCHEMA + DROP SCHEMA registry.
//! Validates the catalog round-trip + built-in schema protection
//! + prefix-routing semantics for schema-qualified table refs.

use spg_engine::Engine;

#[test]
fn create_schema_registers_it() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    assert!(e.catalog().schema_exists("app"));
    assert!(e.catalog().user_schemas().contains("app"));
}

#[test]
fn builtin_schemas_always_exist_without_create() {
    let e = Engine::new();
    assert!(e.catalog().schema_exists("public"));
    assert!(e.catalog().schema_exists("pg_catalog"));
    assert!(e.catalog().schema_exists("information_schema"));
    // But they are NOT in user_schemas.
    assert!(e.catalog().user_schemas().is_empty());
}

#[test]
fn duplicate_create_schema_errors() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    let err = e.execute("CREATE SCHEMA app");
    assert!(err.is_err());
}

#[test]
fn create_schema_if_not_exists_is_silent_on_duplicate() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    e.execute("CREATE SCHEMA IF NOT EXISTS app").unwrap();
}

#[test]
fn create_schema_rejects_builtin_name() {
    let mut e = Engine::new();
    let err = e.execute("CREATE SCHEMA public");
    assert!(err.is_err());
    let err2 = e.execute("CREATE SCHEMA pg_catalog");
    assert!(err2.is_err());
}

#[test]
fn create_schema_if_not_exists_silent_on_builtin() {
    let mut e = Engine::new();
    // IF NOT EXISTS on a built-in is a silent no-op — PG-strict.
    e.execute("CREATE SCHEMA IF NOT EXISTS public").unwrap();
}

#[test]
fn drop_schema_removes_user_entry() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    e.execute("DROP SCHEMA app").unwrap();
    assert!(!e.catalog().schema_exists("app"));
}

#[test]
fn drop_schema_rejects_builtin() {
    let mut e = Engine::new();
    let err = e.execute("DROP SCHEMA public");
    assert!(err.is_err(), "built-in schema should not be droppable");
}

#[test]
fn drop_schema_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP SCHEMA IF EXISTS does_not_exist").unwrap();
}

#[test]
fn schema_qualified_table_strips_prefix_at_lookup() {
    // v7.17.0 Phase 1.6 is prefix routing, not isolation:
    // `app.users` and `analytics.users` both resolve to the
    // bare `users` table. This pin test documents the
    // behaviour so the v7.18+ isolation work has a clear before
    // state.
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    e.execute("CREATE TABLE app.users (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO app.users VALUES (1)").unwrap();
    // Bare-name lookup works (prefix stripped at CREATE).
    let r = e.execute("SELECT id FROM users").unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    // Different schema also resolves (same prefix-routing rule).
    let r2 = e.execute("SELECT id FROM other_schema.users").unwrap();
    let rows2 = match r2 {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows2.len(), 1);
}

#[test]
fn schema_with_authorization_clause_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app AUTHORIZATION someuser").unwrap();
    assert!(e.catalog().schema_exists("app"));
}

#[test]
fn schemas_round_trip_catalog() {
    let mut e = Engine::new();
    e.execute("CREATE SCHEMA app").unwrap();
    e.execute("CREATE SCHEMA analytics").unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    assert!(restored.schema_exists("app"));
    assert!(restored.schema_exists("analytics"));
    assert!(restored.schema_exists("public"));
    assert_eq!(restored.user_schemas().len(), 2);
}
