//! v7.37.17 (17.6 siblings) — to_regclass / to_regtype /
//! to_regnamespace upgraded from NULL stubs to real resolvers.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn to_regclass_existence_check() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE reg_t (id INT)").unwrap();
    // The Django/Alembic existence-check shape.
    // v7.39 (read01 regproc.c) — to_regclass returns a regclass, which
    // renders as the relation name (the IS NOT NULL check still works).
    assert_eq!(
        first(&mut e, "SELECT to_regclass('reg_t')"),
        spg_storage::Value::text("reg_t")
    );
    // 'public.' qualification accepted.
    assert_eq!(
        first(&mut e, "SELECT to_regclass('public.reg_t')"),
        spg_storage::Value::text("reg_t")
    );
    // Missing relation → NULL (never an error — that's the point
    // of to_regclass vs a regclass cast).
    assert!(matches!(
        first(&mut e, "SELECT to_regclass('no_such_table')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn to_regclass_resolves_views() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE vt (id INT)").unwrap();
    e.execute("CREATE VIEW v_vt AS SELECT id FROM vt").unwrap();
    assert_eq!(
        first(&mut e, "SELECT to_regclass('v_vt')"),
        spg_storage::Value::text("v_vt")
    );
}

#[test]
fn to_regtype_builtin_names() {
    let mut e = Engine::new();
    assert_eq!(
        first(&mut e, "SELECT to_regtype('integer')"),
        spg_storage::Value::text("integer")
    );
    assert_eq!(
        first(&mut e, "SELECT to_regtype('character varying')"),
        spg_storage::Value::text("character varying")
    );
    assert_eq!(
        first(&mut e, "SELECT to_regtype('jsonb')"),
        spg_storage::Value::text("jsonb")
    );
    assert!(matches!(
        first(&mut e, "SELECT to_regtype('no_such_type')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn to_regnamespace_known_schemas() {
    let mut e = Engine::new();
    assert_eq!(
        first(&mut e, "SELECT to_regnamespace('public')"),
        spg_storage::Value::text("public")
    );
    assert_eq!(
        first(&mut e, "SELECT to_regnamespace('pg_catalog')"),
        spg_storage::Value::text("pg_catalog")
    );
    assert!(matches!(
        first(&mut e, "SELECT to_regnamespace('nope')"),
        spg_storage::Value::Null
    ));
}
