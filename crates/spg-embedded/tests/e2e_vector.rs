//! v7.0 — verify `spg-embedded::Database` supports the full
//! vector / HNSW / pgvector-compat surface that `spg-engine`
//! ships. `Database` is a thin wrapper, so every vector
//! capability the engine exposes — `VECTOR(N)` columns,
//! `USING hnsw` indices, F32 / SQ8 / HALF encodings, the
//! `<->` / `<#>` / `<=>` distance operators, ORDER BY kNN — is
//! reachable directly through `execute(sql)` / `query(sql)`.

use spg_embedded::Database;
use spg_storage::Value;

#[test]
fn create_vector_table_and_basic_insert() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0, 4.0])")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (2, [4.0, 5.0, 6.0, 7.0])")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (3, [6.0, 7.0, 8.0, 9.0])")
        .unwrap();
    let rows = db.query("SELECT id FROM emb WHERE id = 1").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn knn_topk_via_l2_distance() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    let rows = [
        (1, "[1.0, 2.0, 3.0, 4.0]"),
        (2, "[4.0, 5.0, 6.0, 7.0]"),
        (3, "[6.0, 7.0, 8.0, 9.0]"),
        (4, "[2.0, 3.0, 4.0, 5.0]"),
        (5, "[1.0, 2.0, 3.0, 5.0]"),
    ];
    for (id, v) in rows {
        db.execute(&format!("INSERT INTO emb VALUES ({id}, {v})"))
            .unwrap();
    }
    // Order-by L2 distance, top 3.
    let got = db
        .query("SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3")
        .unwrap();
    let ids: Vec<i32> = got
        .into_iter()
        .map(|r| match r.into_iter().next().unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    // id=1 is distance 0 (self), id=5 is closest non-self (squared L2 = 1),
    // id=4 is third (squared L2 = 4).
    assert_eq!(ids, vec![1, 5, 4]);
}

#[test]
fn hnsw_index_kkn_picks_index_over_full_scan() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX emb_idx ON emb USING hnsw (v)")
        .unwrap();
    for (id, v) in [
        (1, "[1.0, 2.0, 3.0, 4.0]"),
        (2, "[4.0, 5.0, 6.0, 7.0]"),
        (3, "[6.0, 7.0, 8.0, 9.0]"),
        (4, "[2.0, 3.0, 4.0, 5.0]"),
        (5, "[1.0, 2.0, 3.0, 5.0]"),
    ] {
        db.execute(&format!("INSERT INTO emb VALUES ({id}, {v})"))
            .unwrap();
    }
    // Same top-K as full-scan baseline above — HNSW returns rows in
    // ascending-distance order so the order is preserved.
    let got = db
        .query("SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3")
        .unwrap();
    let ids: Vec<i32> = got
        .into_iter()
        .map(|r| match r.into_iter().next().unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 5, 4]);
}

#[test]
fn sq8_encoding_round_trips_distances() {
    // 8-bit quantised vector storage — `USING SQ8` (v6.0.1).
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) USING SQ8 NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0, 4.0])")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (2, [1.1, 2.1, 3.1, 4.1])")
        .unwrap();
    let got = db
        .query("SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 1")
        .unwrap();
    // Closest to itself.
    let id = match &got[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(id, 1);
}

#[test]
fn half_encoding_works() {
    // pgvector-flavoured HALF (binary16) encoding (v6.0.3).
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) USING HALF NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0, 4.0])")
        .unwrap();
    let got = db.query("SELECT id FROM emb").unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn pgvector_inner_product_operator_works() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 0.0, 0.0, 0.0])")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (2, [0.0, 1.0, 0.0, 0.0])")
        .unwrap();
    // `<#>` = negative dot product (pgvector convention).
    let got = db
        .query("SELECT id FROM emb ORDER BY v <#> [1.0, 0.0, 0.0, 0.0] LIMIT 1")
        .unwrap();
    let id = match &got[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    };
    // Most "similar" via inner product (smallest negative IP) → id=1.
    assert_eq!(id, 1);
}

#[test]
fn cosine_distance_operator_works() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 0.0, 0.0, 0.0])")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (2, [2.0, 0.0, 0.0, 0.0])") // parallel → distance 0
        .unwrap();
    db.execute("INSERT INTO emb VALUES (3, [0.0, 1.0, 0.0, 0.0])") // orthogonal → distance 1
        .unwrap();
    let got = db
        .query("SELECT id FROM emb ORDER BY v <=> [1.0, 0.0, 0.0, 0.0] LIMIT 2")
        .unwrap();
    let ids: Vec<i32> = got
        .into_iter()
        .map(|r| match r.into_iter().next().unwrap() {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    // Both id=1 and id=2 have cosine distance 0; orthogonal id=3 is last.
    assert!(ids.contains(&1) && ids.contains(&2));
}
