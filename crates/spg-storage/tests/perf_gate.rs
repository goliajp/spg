// Test-gate allow-list — see crates/spg-crypto/tests/perf_gate.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::items_after_statements
)]

//! Regression-catch perf gate for `spg-storage`. Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_storage::{
    Catalog, ColumnSchema, DataType, NSW_DEFAULT_M, NswMetric, Row, TableSchema, Value, nsw_query,
    persistent::PersistentVec,
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

/// v4.38 BVT push 性能门 — 1M `u64` push 操作 ≤ 200 ms。
/// 实测在 M-series mac release 模式 ~80-120 ms；200 ms 留 ~2× 余量给 CI / Linux host。
#[test]
fn pv_push_1m_under_200ms() {
    let start = Instant::now();
    let mut pv: PersistentVec<u64> = PersistentVec::new();
    for i in 0..1_000_000_u64 {
        pv = pv.push(i);
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&pv);
    let budget_ms: u128 = 200;
    let elapsed_ms = elapsed.as_millis();
    assert!(
        elapsed_ms < budget_ms,
        "pv_push_1m elapsed {elapsed_ms} ms exceeds budget {budget_ms} ms"
    );
    assert_eq!(pv.len(), 1_000_000);
}

/// v4.39.1 transient push 性能门 — 1M 次 `push_mut` ≤ 50 ms。`push` 每次 path-copy
/// tail（O(BRANCH) 上限，对 `T = Row` ~600 ns/row），所以 200 ms gate（v4.38）反映的是
/// `push` 的成本。`push_mut` 用 `Arc::make_mut` 在 Arc 唯一拥有时直接 in-place mutate
/// tail，恢复 `Vec::push` 同级 ~10-30 ns/row。50 ms gate 留 ~2× 余量。
/// 这是 spg-embedded 流式 INSERT 路径恢复 baseline 吞吐的关键。
#[test]
fn pv_push_mut_1m_under_50ms() {
    let start = Instant::now();
    let mut pv: PersistentVec<u64> = PersistentVec::new();
    for i in 0..1_000_000_u64 {
        pv.push_mut(i);
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&pv);
    let budget_ms: u128 = 50;
    let elapsed_ms = elapsed.as_millis();
    assert!(
        elapsed_ms < budget_ms,
        "pv_push_mut_1m elapsed {elapsed_ms} ms exceeds budget {budget_ms} ms"
    );
    assert_eq!(pv.len(), 1_000_000);
}

/// v4.38 BVT random `get` 性能门 — 1M 元素的 PV 上随机 `get` 平均 ≤ 100 ns。
/// 实测 ~30-60 ns；100 ns 同样留 ~2× 余量。100K 次采样平摊掉 Instant 噪声。
#[test]
fn pv_get_random_under_100ns_avg() {
    let mut pv: PersistentVec<u64> = PersistentVec::new();
    for i in 0..1_000_000_u64 {
        pv = pv.push(i);
    }
    // SplitMix-style scrambler — same shape used in fuzz oracle / NSW level
    // assignment. Reproducible without `rand`.
    let mut state: u64 = 0xC0FFEE_u64;
    const N_PROBES: usize = 100_000;
    let start = Instant::now();
    let mut acc: u64 = 0;
    for _ in 0..N_PROBES {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = state;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let idx = (x as usize) % 1_000_000;
        let v = pv.get(std::hint::black_box(idx)).unwrap();
        acc = acc.wrapping_add(*v);
    }
    let elapsed = start.elapsed();
    std::hint::black_box(acc);
    let avg_ns = elapsed.as_nanos() / (N_PROBES as u128);
    let budget_ns: u128 = 100;
    assert!(
        avg_ns < budget_ns,
        "pv_get_random avg {avg_ns} ns exceeds budget {budget_ns} ns"
    );
}
