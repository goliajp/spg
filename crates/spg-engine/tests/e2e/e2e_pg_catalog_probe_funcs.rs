//! v7.37.17 (17.6 siblings) — pg_catalog probe helpers ORMs /
//! monitoring exporters commonly call.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn ddl_reconstruction_funcs_return_null_or_admin() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_get_viewdef(1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_functiondef(1)"),
        spg_storage::Value::Null
    ));
    // v7.38 (read01) — pg_get_expr now passes through its text first arg:
    // SPG's pg_attrdef.adbin holds the already-deparsed default text (no real
    // pg_node_tree), so `pg_get_expr(adbin, adrelid)` returns it verbatim.
    match first(&mut e, "SELECT pg_get_expr('x', 1)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "x"),
        other => panic!("expected 'x', got {other:?}"),
    }
    match first(&mut e, "SELECT pg_get_userbyid(10)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "admin"),
        other => panic!("expected admin, got {other:?}"),
    }
}

#[test]
fn size_and_encoding_funcs_return_defaults() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_database_size('spg')") {
        spg_storage::Value::BigInt(0) => {}
        other => panic!("expected 0, got {other:?}"),
    }
    match first(&mut e, "SELECT pg_encoding_to_char(6)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "UTF8"),
        other => panic!("expected UTF8, got {other:?}"),
    }
    match first(&mut e, "SELECT pg_char_to_encoding('UTF8')") {
        spg_storage::Value::Int(6) => {}
        other => panic!("expected 6, got {other:?}"),
    }
}

#[test]
fn permission_probes_return_true() {
    let mut e = Engine::new();
    for f in &[
        "has_table_privilege('foo', 'select')",
        "has_column_privilege('foo', 'col', 'select')",
        "has_schema_privilege('public', 'usage')",
        "has_database_privilege('spg', 'connect')",
    ] {
        match first(&mut e, &format!("SELECT {f}")) {
            spg_storage::Value::Bool(true) => {}
            other => panic!("SELECT {f} expected true, got {other:?}"),
        }
    }
}

#[test]
fn admin_signal_funcs_return_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_cancel_backend(1)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("expected true, got {other:?}"),
    }
    match first(&mut e, "SELECT pg_terminate_backend(1)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("expected true, got {other:?}"),
    }
    // pg_backend_pid returns an integer.
    match first(&mut e, "SELECT pg_backend_pid()") {
        spg_storage::Value::Int(_) => {}
        other => panic!("expected Int, got {other:?}"),
    }
}
