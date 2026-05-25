// Test-gate allow-list — see crates/spg-crypto/tests/perf_gate.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Regression-catch perf gate for `spg-storage`. Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_storage::{
    Catalog, ColumnSchema, DataType, NSW_DEFAULT_M, NswMetric, Row, TableSchema, Value, nsw_query,
};

fn build_catalog(n_rows: i32) -> Catalog {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("score", DataType::Float, true),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..n_rows {
        t.insert(Row::new(vec![
            Value::Int(i),
            Value::Text(format!("user-{i}")),
            Value::Float(f64::from(i) * 0.1),
        ]))
        .unwrap();
    }
    cat
}

#[test]
fn catalog_roundtrip_100rows_under_budget() {
    let cat = build_catalog(100);
    let bytes = cat.serialize();
    let iters: u32 = 100;
    let start = Instant::now();
    for _ in 0..iters {
        let out = cat.serialize();
        let restored = Catalog::deserialize(std::hint::black_box(&out)).expect("deserialize ok");
        std::hint::black_box(restored);
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 5e-3;
    assert!(
        mean_secs < budget_secs,
        "catalog_roundtrip_100rows mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
    let _ = bytes;
}

#[test]
fn hnsw_search_under_budget() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("v", DataType::Vector(8), false),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for i in 0_i32..200 {
        #[allow(clippy::cast_precision_loss)]
        let f = i as f32;
        t.insert(Row::new(vec![
            Value::Int(i),
            Value::Vector(vec![
                f * 0.01,
                f * 0.02,
                f * 0.03,
                f * 0.04,
                f * 0.05,
                f * 0.06,
                f * 0.07,
                f * 0.08,
            ]),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    let table = cat.get("vecs").unwrap();
    let query = vec![1.5_f32, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0];
    let iters: u32 = 200;
    let start = Instant::now();
    for _ in 0..iters {
        let hits = nsw_query(
            std::hint::black_box(table),
            "v_idx",
            std::hint::black_box(&query),
            10,
            NswMetric::L2,
        );
        std::hint::black_box(hits);
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 1e-3;
    assert!(
        mean_secs < budget_secs,
        "hnsw_search_top10_dim8_n200 mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
