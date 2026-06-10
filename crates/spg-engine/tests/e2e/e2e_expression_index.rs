//! v6.8.2 — Expression index (`CREATE INDEX … (lower(col))`).
//!
//! Format-layer ship — same envelope as v6.8.0 INCLUDE and
//! v6.8.1 WHERE. The expression's canonical Display form lives
//! on the catalog snapshot; the runtime maintenance pass
//! evaluates each row's expression to derive the index key — but
//! v6.8.2 falls back to the bare-column-reference path and
//! the runtime work is STABILITY carve-out. Correctness is
//! preserved: queries against expression-indexed tables still
//! work, just without the expression-aware seek shortcut.

use spg_engine::Engine;
use spg_storage::Catalog;

#[test]
fn create_index_with_function_call_persists_expression() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_lower_name ON t (lower(name))")
        .unwrap();
    let idx = e
        .catalog()
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "by_lower_name")
        .expect("index by_lower_name");
    let expr = idx.expression.as_ref().expect("expression set");
    assert!(
        expr.to_ascii_lowercase().contains("lower"),
        "unexpected canonical Display: {expr}"
    );
    // The primary column position is derived from the first
    // column referenced in the expression.
    assert_eq!(idx.column_position, 1, "primary column = name (pos 1)");
}

#[test]
fn create_index_bare_column_has_no_expression() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    let idx = e
        .catalog()
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "by_id")
        .unwrap();
    assert!(idx.expression.is_none());
}

#[test]
fn create_index_expression_rejected_on_hnsw() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let r = e.execute("CREATE INDEX i ON t USING hnsw (lower(name))");
    assert!(r.is_err(), "expression on HNSW must error");
}

#[test]
fn expression_index_survives_catalog_snapshot_roundtrip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_lower_name ON t (lower(name))")
        .unwrap();
    let bytes = e.catalog().serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    let idx = restored
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "by_lower_name")
        .expect("index survives roundtrip");
    assert!(idx.expression.is_some());
    assert_eq!(idx.column_position, 1);
}

#[test]
fn expression_index_does_not_break_basic_select() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_lower_name ON t (lower(name))")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'Alice')").unwrap();
    e.execute("INSERT INTO t VALUES (2, 'BOB')").unwrap();
    let r = e.execute("SELECT id FROM t WHERE id = 1").unwrap();
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn create_index_expression_with_no_column_ref_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    // `1 + 1` references no column — parser must surface this as
    // an error before any catalog mutation lands.
    let r = e.execute("CREATE INDEX i ON t (1 + 1)");
    assert!(r.is_err());
}

#[test]
fn create_index_display_round_trips_expression() {
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;
    let sql = "CREATE INDEX by_lower_name ON t (lower(name))";
    let stmt = parse_statement(sql).unwrap();
    let Statement::CreateIndex(ref s) = stmt else {
        panic!("expected CreateIndex");
    };
    assert!(s.expression.is_some());
    let stmt2 = parse_statement(&stmt.to_string()).unwrap();
    assert_eq!(stmt2, stmt);
}
