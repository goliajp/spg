//! v7.17.0 Phase 1.2 — CREATE / DROP VIEW + SELECT FROM view.
//! Validates the catalog round-trip + the expand_views_in_select
//! rewrite that prepends each referenced view as a synthetic CTE
//! before the regular CTE materialiser runs.

use spg_engine::{Engine, QueryResult};

fn rows(r: QueryResult) -> Vec<Vec<spg_storage::Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn create_view_and_select_returns_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    e.execute("CREATE VIEW v AS SELECT id, name FROM t WHERE id > 1")
        .unwrap();
    let r = rows(e.execute("SELECT id, name FROM v").unwrap());
    assert_eq!(r.len(), 2);
    let names: Vec<&str> = r
        .iter()
        .map(|row| match &row[1] {
            spg_storage::Value::Text(s) => s.as_ref(),
            other => panic!("not text: {other:?}"),
        })
        .collect();
    assert!(names.contains(&"b"));
    assert!(names.contains(&"c"));
}

#[test]
fn create_view_with_column_rename_list() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    e.execute("CREATE VIEW v (pk, label) AS SELECT id, name FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT pk, label FROM v").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0][0],
        spg_storage::Value::Int(1),
        "pk column should select id"
    );
    assert_eq!(
        r[0][1],
        spg_storage::Value::text("alice"),
        "label column should select name"
    );
}

#[test]
fn create_or_replace_view_overwrites() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    e.execute("CREATE VIEW v AS SELECT id FROM t WHERE id = 1")
        .unwrap();
    let r1 = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r1.len(), 1);
    e.execute("CREATE OR REPLACE VIEW v AS SELECT id FROM t WHERE id >= 1")
        .unwrap();
    let r2 = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r2.len(), 2);
}

#[test]
fn create_view_if_not_exists_noop() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE VIEW v AS SELECT id FROM t").unwrap();
    e.execute("CREATE VIEW IF NOT EXISTS v AS SELECT 999 AS id FROM t")
        .unwrap();
    // Original definition kept.
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], spg_storage::Value::Int(1));
}

#[test]
fn create_view_rejects_collision_with_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let err = e.execute("CREATE VIEW t AS SELECT id FROM t");
    assert!(err.is_err(), "view should not shadow table");
}

#[test]
fn drop_view_removes_access() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE VIEW v AS SELECT id FROM t").unwrap();
    e.execute("SELECT id FROM v").unwrap();
    e.execute("DROP VIEW v").unwrap();
    let err = e.execute("SELECT id FROM v");
    assert!(err.is_err(), "SELECT FROM dropped view should error");
}

#[test]
fn drop_view_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP VIEW IF EXISTS missing").unwrap();
}

#[test]
fn view_with_aggregate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (k INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 30)")
        .unwrap();
    e.execute("CREATE VIEW v AS SELECT k, sum(v) AS total FROM t GROUP BY k")
        .unwrap();
    let r = rows(e.execute("SELECT k, total FROM v ORDER BY k").unwrap());
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], spg_storage::Value::Int(1));
    assert_eq!(r[1][0], spg_storage::Value::Int(2));
}

#[test]
fn view_joined_with_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE customers (id INT NOT NULL, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO customers VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    e.execute("CREATE TABLE orders (id INT NOT NULL, customer_id INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (10, 1), (11, 1), (12, 2)")
        .unwrap();
    e.execute("CREATE VIEW alice_orders AS SELECT id FROM orders WHERE customer_id = 1")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT customers.name, alice_orders.id FROM customers \
             INNER JOIN alice_orders ON alice_orders.id = alice_orders.id \
             WHERE customers.id = 1 ORDER BY alice_orders.id",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], spg_storage::Value::text("alice"));
}

#[test]
fn nested_view_view_references_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, k INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)")
        .unwrap();
    e.execute("CREATE VIEW v1 AS SELECT id, k FROM t WHERE k >= 200")
        .unwrap();
    e.execute("CREATE VIEW v2 AS SELECT id FROM v1 WHERE k >= 300")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v2").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], spg_storage::Value::Int(3));
}

#[test]
fn catalog_round_trip_preserves_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE VIEW v AS SELECT id FROM t").unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let view = restored.view("v").expect("view persisted");
    assert!(view.body.to_ascii_lowercase().contains("select"));
    assert!(view.body.contains("t"));
}
