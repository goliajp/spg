//! v7.17.0 Phase 1.3 — CREATE MATERIALIZED VIEW + REFRESH + DROP.
//! Validates the cache-at-CREATE / re-eval-at-REFRESH semantics
//! over the regular table backing.

use spg_engine::{Engine, QueryResult};

fn rows(r: QueryResult) -> Vec<Vec<spg_storage::Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn create_materialized_view_caches_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t WHERE v >= 200")
        .unwrap();
    let r = rows(e.execute("SELECT id, v FROM mv ORDER BY id").unwrap());
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], spg_storage::Value::Int(2));
    assert_eq!(r[1][0], spg_storage::Value::Int(3));
}

#[test]
fn materialized_view_does_not_see_post_create_changes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    // Mutate source after materialisation.
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    // Materialised view still shows the pre-INSERT cache.
    let r = rows(e.execute("SELECT id FROM mv").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], spg_storage::Value::Int(1));
}

#[test]
fn refresh_materialized_view_picks_up_changes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    e.execute("INSERT INTO t VALUES (2), (3)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
    let r = rows(e.execute("SELECT id FROM mv ORDER BY id").unwrap());
    assert_eq!(r.len(), 3);
    assert_eq!(r[2][0], spg_storage::Value::Int(3));
}

#[test]
fn refresh_with_no_data_truncates() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    assert_eq!(rows(e.execute("SELECT id FROM mv").unwrap()).len(), 2);
    e.execute("REFRESH MATERIALIZED VIEW mv WITH NO DATA")
        .unwrap();
    assert_eq!(rows(e.execute("SELECT id FROM mv").unwrap()).len(), 0);
}

#[test]
fn create_with_no_data_starts_empty() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t WITH NO DATA")
        .unwrap();
    assert_eq!(rows(e.execute("SELECT id FROM mv").unwrap()).len(), 0);
    e.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
    assert_eq!(rows(e.execute("SELECT id FROM mv").unwrap()).len(), 2);
}

#[test]
fn drop_materialized_view_removes_backing_table_and_source() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    e.execute("DROP MATERIALIZED VIEW mv").unwrap();
    // Backing table gone.
    assert!(e.execute("SELECT id FROM mv").is_err());
    // Source registry gone.
    assert!(e.execute("REFRESH MATERIALIZED VIEW mv").is_err());
}

#[test]
fn drop_materialized_view_if_exists_silent() {
    let mut e = Engine::new();
    e.execute("DROP MATERIALIZED VIEW IF EXISTS missing")
        .unwrap();
}

/// v7.37.19 (19.8) — REFRESH MATERIALIZED VIEW CONCURRENTLY parses
/// and executes identically to the non-CONCURRENTLY form. SPG
/// materialised views re-evaluate on read (always-fresh semantics),
/// so the CONCURRENTLY-vs-serial distinction has no runtime effect.
#[test]
fn refresh_materialized_view_concurrently_parses() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    e.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv")
        .unwrap();
    e.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv WITH NO DATA")
        .unwrap();
}

#[test]
fn create_materialized_view_with_column_rename() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv (pk, label) AS SELECT id, name FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT pk, label FROM mv").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], spg_storage::Value::Int(1));
    assert_eq!(r[0][1], spg_storage::Value::text("alice"));
}

#[test]
fn create_materialized_view_collision_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mv (id INT NOT NULL)").unwrap();
    let err = e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM mv");
    assert!(err.is_err());
}

#[test]
fn create_if_not_exists_is_noop_on_existing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS SELECT 999 AS id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM mv").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], spg_storage::Value::Int(1));
}

#[test]
fn catalog_round_trip_preserves_matview() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    assert!(restored.materialized_views().contains_key("mv"));
    assert!(restored.get("mv").is_some());
}
