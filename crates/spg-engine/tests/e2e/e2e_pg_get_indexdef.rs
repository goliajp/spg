//! v7.37.17 (17.6 siblings) — pg_get_indexdef upgraded from NULL
//! stub to real catalog reconstruction.

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
fn indexdef_reconstructs_create_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ti (id INT, name TEXT)").unwrap();
    e.execute("CREATE INDEX idx_ti_name ON ti (name)").unwrap();
    let def = text(&first(&mut e, "SELECT pg_get_indexdef('idx_ti_name')"));
    assert_eq!(def, "CREATE INDEX idx_ti_name ON public.ti (name)");
    // Matches the pg_indexes view's construction exactly.
    let view_def = text(&first(
        &mut e,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'idx_ti_name'",
    ));
    assert_eq!(def, view_def);
}

#[test]
fn indexdef_unique_keyword() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tu (email TEXT)").unwrap();
    e.execute("CREATE UNIQUE INDEX idx_tu_email ON tu (email)")
        .unwrap();
    let def = text(&first(&mut e, "SELECT pg_get_indexdef('idx_tu_email')"));
    assert!(def.starts_with("CREATE UNIQUE INDEX"), "def: {def}");
}

#[test]
fn indexdef_column_form() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tc (a INT, b TEXT)").unwrap();
    e.execute("CREATE INDEX idx_tc_b ON tc (b)").unwrap();
    // 3-arg-ish column form: column 1 → the indexed column name.
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_indexdef('idx_tc_b', 1)")),
        "b"
    );
    // Beyond the single column → NULL.
    assert!(matches!(
        first(&mut e, "SELECT pg_get_indexdef('idx_tc_b', 2)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn indexdef_unknown_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_get_indexdef('no_such_index')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_indexdef(NULL::text)"),
        spg_storage::Value::Null
    ));
}
