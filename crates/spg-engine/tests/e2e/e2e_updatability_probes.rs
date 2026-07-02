//! v7.37.17 (17.6 siblings) — updatability probes:
//! pg_relation_is_updatable / pg_column_is_updatable /
//! row_security_active.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn relation_is_updatable_bitmask() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ut (id INT)").unwrap();
    e.execute("CREATE VIEW uv AS SELECT id FROM ut").unwrap();
    // 28 = INSERT(8) | UPDATE(4) | DELETE(16) — fully updatable.
    assert!(matches!(
        first(&mut e, "SELECT pg_relation_is_updatable('ut', false)"),
        spg_storage::Value::Int(28)
    ));
    // Views are auto-updatable in SPG (v7.37.19 redirect).
    assert!(matches!(
        first(&mut e, "SELECT pg_relation_is_updatable('uv', false)"),
        spg_storage::Value::Int(28)
    ));
    // Missing relation → 0.
    assert!(matches!(
        first(&mut e, "SELECT pg_relation_is_updatable('nope', false)"),
        spg_storage::Value::Int(0)
    ));
}

#[test]
fn column_is_updatable_and_rls() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE uc (id INT)").unwrap();
    assert!(matches!(
        first(&mut e, "SELECT pg_column_is_updatable('uc', 1, false)"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_column_is_updatable('nope', 1, false)"),
        spg_storage::Value::Bool(false)
    ));
    // No row-level security in SPG.
    assert!(matches!(
        first(&mut e, "SELECT row_security_active('uc')"),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn updatability_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "pg_relation_is_updatable(NULL::text, false)",
        "pg_column_is_updatable(NULL::text, 1, false)",
        "row_security_active(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
