//! v7.37.17 (17.6 siblings) — pg_get_viewdef upgraded from NULL
//! stub to real catalog lookup.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn viewdef_returns_body() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    e.execute("CREATE VIEW v_names AS SELECT name FROM t WHERE id > 0")
        .unwrap();
    let def = text(&first(&mut e, "SELECT pg_get_viewdef('v_names')"));
    assert!(
        def.to_lowercase().contains("select") && def.contains("name"),
        "viewdef: {def}"
    );
    // 'public.' qualification accepted.
    let def2 = text(&first(
        &mut e,
        "SELECT pg_get_viewdef('public.v_names')",
    ));
    assert_eq!(def, def2);
    // Pretty flag accepted + ignored.
    let def3 = text(&first(
        &mut e,
        "SELECT pg_get_viewdef('v_names', true)",
    ));
    assert_eq!(def, def3);
}

#[test]
fn viewdef_unknown_view_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_get_viewdef('no_such_view')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_viewdef(NULL::text)"),
        spg_storage::Value::Null
    ));
}
