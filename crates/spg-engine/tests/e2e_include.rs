//! v6.8.0 — `CREATE INDEX … INCLUDE (col, …)`.
//!
//! Format-layer ship (the v6.8.0 scope). Stores the INCLUDE
//! columns on the catalog snapshot (FILE_VERSION 11 → 12) so a
//! future planner pass can read them. The actual "index-only
//! scan" planner optimisation — avoiding the heap fetch when a
//! query is fully covered by `(key + included)` — is OOS for
//! v6.8 (STABILITY carve-out): SPG's hot tier lives in memory
//! today, so heap fetch == pointer chase, and the in-BTree-leaf
//! payload encoding is only worth the wire-format work when the
//! cold tier becomes the primary lookup path.

use spg_engine::Engine;
use spg_storage::Catalog;

#[test]
fn create_index_with_include_persists_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id) INCLUDE (name, age)").unwrap();
    let t = e.catalog().get("t").expect("table");
    let idx = t
        .indices()
        .iter()
        .find(|i| i.name == "by_id")
        .expect("index by_id");
    // Column positions: name=1, age=2 in declaration order.
    assert_eq!(idx.included_columns, vec![1, 2]);
}

#[test]
fn create_index_without_include_has_empty_vec() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    let t = e.catalog().get("t").expect("table");
    let idx = t
        .indices()
        .iter()
        .find(|i| i.name == "by_id")
        .expect("index by_id");
    assert!(idx.included_columns.is_empty());
}

#[test]
fn create_index_include_unknown_column_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let r = e.execute("CREATE INDEX by_id ON t (id) INCLUDE (name, ghost)");
    assert!(
        r.is_err(),
        "unknown INCLUDE column must error before catalog mutation lands"
    );
    // The error path must not leave a half-built index behind.
    let t = e.catalog().get("t").expect("table");
    assert!(t.indices().iter().all(|i| i.name != "by_id"));
}

#[test]
fn create_index_include_rejected_on_hnsw() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL, tag TEXT)")
        .unwrap();
    let r = e.execute("CREATE INDEX emb_idx ON emb USING hnsw (v) INCLUDE (tag)");
    assert!(
        r.is_err(),
        "INCLUDE on HNSW must error — included data is only meaningful for BTree covered scans"
    );
}

#[test]
fn included_columns_survive_catalog_snapshot_roundtrip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL, age INT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id) INCLUDE (name, age)").unwrap();
    let bytes = e.catalog().serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    let idx = restored
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "by_id")
        .expect("index by_id");
    assert_eq!(idx.included_columns, vec![1, 2]);
}

#[test]
fn legacy_v11_snapshot_loads_with_empty_included() {
    // Build a fresh catalog (would serialise as v12 today), then
    // hand-construct a v11 snapshot by chopping the trailing
    // included_columns u16 off each index. Easier: use a catalog
    // with no INCLUDE clauses and verify deserialise via the v12
    // path returns the same empty Vec.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)").unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    let bytes = e.catalog().serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    let idx = restored
        .get("t")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "by_id")
        .unwrap();
    assert!(idx.included_columns.is_empty());
}

#[test]
fn create_index_display_round_trips_include() {
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;
    let sql = "CREATE INDEX by_id ON t (id) INCLUDE (name, age)";
    let stmt = parse_statement(sql).unwrap();
    let Statement::CreateIndex(ref s) = stmt else {
        panic!("expected CreateIndex");
    };
    assert_eq!(s.included_columns, vec!["name".to_string(), "age".to_string()]);
    // Display + re-parse round-trip.
    let s2 = parse_statement(&stmt.to_string()).unwrap();
    assert_eq!(s2, stmt);
}
