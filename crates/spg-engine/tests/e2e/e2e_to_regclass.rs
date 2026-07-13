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
    // v7.39 (read01 ruleutils.c) — to_regclass returns a DUAL-shape
    // regclass (oid for joins, name for display); it renders as the name
    // and the IS NOT NULL existence check still works.
    let v = first(&mut e, "SELECT to_regclass('reg_t')");
    assert_eq!(spg_engine::eval::value_to_text(&v), "reg_t");
    assert!(matches!(v, spg_storage::Value::RegClass(oid, _) if oid >= 16384));
    // 'public.' qualification accepted.
    let v2 = first(&mut e, "SELECT to_regclass('public.reg_t')");
    assert_eq!(spg_engine::eval::value_to_text(&v2), "reg_t");
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
    let v = first(&mut e, "SELECT to_regclass('v_vt')");
    assert_eq!(spg_engine::eval::value_to_text(&v), "v_vt");
    assert!(matches!(v, spg_storage::Value::RegClass(oid, _) if oid >= 32768));
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
