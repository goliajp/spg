//! v6.8.1 — `CREATE INDEX … WHERE <expr>` (partial index).
//!
//! Format-layer ship — same scope envelope as v6.8.0 INCLUDE.
//! The WHERE predicate's canonical Display form is stored on the
//! catalog snapshot (FILE_VERSION 12); the engine over-maintains
//! partial indexes (same maintenance path as full indexes), and
//! the planner-side "use partial when query WHERE implies the
//! predicate" pass is STABILITY carve-out (cold tier query
//! parallelism, scan-triggered prefetch, BRIN page-skipping —
//! same shape).

use spg_engine::Engine;
use spg_storage::Catalog;

#[test]
fn create_index_with_where_persists_predicate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, active INT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX active_only ON t (id) WHERE active = 1")
        .unwrap();
    let t = e.catalog().get("t").expect("table");
    let idx = t
        .indices()
        .iter()
        .find(|i| i.name == "active_only")
        .expect("index");
    let pred = idx
        .partial_predicate
        .as_ref()
        .expect("partial predicate set");
    assert!(
        pred.contains("active") && pred.contains("1"),
        "unexpected canonical Display: {pred}"
    );
}

#[test]
fn create_index_without_where_has_no_predicate() {
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
    assert!(idx.partial_predicate.is_none());
}

#[test]
fn create_index_where_rejected_on_hnsw() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL, active INT NOT NULL)")
        .unwrap();
    let r = e.execute("CREATE INDEX emb_idx ON emb USING hnsw (v) WHERE active = 1");
    assert!(r.is_err(), "WHERE on HNSW must error");
}

#[test]
fn partial_predicate_survives_catalog_snapshot_roundtrip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, status TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX live ON t (id) WHERE status = 'live'")
        .unwrap();
    let bytes = e.catalog().serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    let idx = restored
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "live")
        .expect("index live");
    let pred = idx.partial_predicate.as_ref().expect("predicate present");
    assert!(
        pred.contains("status") && pred.contains("live"),
        "unexpected canonical Display: {pred}"
    );
}

#[test]
fn partial_index_does_not_break_basic_select() {
    // Over-maintenance: every inserted row enters the partial
    // index in v6.8.1. Query correctness is preserved because the
    // planner treats partial indexes as candidates the same way
    // as full indexes — the v6.8.x STABILITY carve-out is
    // optimisation, not correctness.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, status TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX live ON t (id) WHERE status = 'live'")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'live')").unwrap();
    e.execute("INSERT INTO t VALUES (2, 'paused')").unwrap();
    let r = e.execute("SELECT id FROM t WHERE id = 1").unwrap();
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows, got {other:?}"),
    }
    let r = e.execute("SELECT id FROM t WHERE id = 2").unwrap();
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn create_index_display_round_trips_where() {
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;
    let sql = "CREATE INDEX live ON t (id) WHERE status = 'live'";
    let stmt = parse_statement(sql).unwrap();
    let Statement::CreateIndex(ref s) = stmt else {
        panic!("expected CreateIndex");
    };
    assert!(s.partial_predicate.is_some());
    // Display + re-parse round-trip.
    let s2 = parse_statement(&stmt.to_string()).unwrap();
    assert_eq!(s2, stmt);
}

#[test]
fn create_index_with_include_and_where_together() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX live ON t (id) INCLUDE (name) WHERE status = 'live'")
        .unwrap();
    let idx = e
        .catalog()
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "live")
        .unwrap();
    assert_eq!(idx.included_columns, vec![1]);
    assert!(idx.partial_predicate.is_some());
}
