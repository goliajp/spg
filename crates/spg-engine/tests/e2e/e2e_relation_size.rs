//! v7.37.17 (17.6 siblings) — pg_relation_size family upgraded from
//! constant-0 stubs to the storage layer's hot-tier byte meter.

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

fn bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn relation_size_grows_with_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sz (id INT, body TEXT)").unwrap();
    let empty = bigint(&first(&mut e, "SELECT pg_relation_size('sz')"));
    e.execute("INSERT INTO sz SELECT g, repeat('x', 100) FROM generate_series(1, 50) g")
        .unwrap();
    let filled = bigint(&first(&mut e, "SELECT pg_relation_size('sz')"));
    assert!(
        filled > empty && filled > 1000,
        "size should grow: empty={empty}, filled={filled}"
    );
    // pg_table_size is the same heap meter.
    assert_eq!(filled, bigint(&first(&mut e, "SELECT pg_table_size('sz')")));
    // total = heap + indexes ≥ heap.
    let total = bigint(&first(&mut e, "SELECT pg_total_relation_size('sz')"));
    assert!(total >= filled);
}

#[test]
fn indexes_size_counts_index_bytes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE isz (id INT)").unwrap();
    e.execute("INSERT INTO isz SELECT g FROM generate_series(1, 100) g")
        .unwrap();
    e.execute("CREATE INDEX idx_isz ON isz (id)").unwrap();
    let idx_bytes = bigint(&first(&mut e, "SELECT pg_indexes_size('isz')"));
    assert!(idx_bytes > 0, "index bytes: {idx_bytes}");
}

#[test]
fn database_size_sums_hot_tier() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dbsz (v TEXT)").unwrap();
    e.execute("INSERT INTO dbsz VALUES (repeat('y', 500))")
        .unwrap();
    let db = bigint(&first(&mut e, "SELECT pg_database_size('spg')"));
    assert!(db > 0, "db size: {db}");
}

#[test]
fn missing_relation_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_relation_size('nope')"),
        spg_storage::Value::Null
    ));
}
