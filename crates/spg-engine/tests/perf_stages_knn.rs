#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::unreadable_literal
)]

//! v6.1.2 perf exploration — stage-level timing for vector kNN.
//!
//! Sits *inside* the engine (no pgwire / TCP / wire encoding) and
//! decomposes a prepared-statement kNN query into its measurable
//! sub-stages so we can see where the 287µs pgwire p50 actually
//! goes. Ignored by default — invoke with
//!   cargo test --release -p spg-engine --test perf_stages_knn \
//!     -- --ignored --nocapture

extern crate alloc;

use std::time::Instant;

use spg_engine::Engine;
use spg_storage::Value;

const DIM: usize = 128;
const ROWS: usize = 1_024;
const N: usize = 1_000;
const WARMUP: usize = 50;
const ROWS_LARGE: usize = 100_000;

fn vec_seed(seed: u64, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    let mut x = seed;
    for _ in 0..dim {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let f = (x >> 40) as f32 / (1u32 << 24) as f32;
        out.push(f);
    }
    out
}

fn vec_text_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{:.4}", x));
    }
    s.push(']');
    s
}

fn pct(samples: &mut [u128], p: f64) -> u128 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * p) as usize;
    samples[idx.min(samples.len() - 1)]
}

fn setup_engine(with_hnsw: bool, rows: usize) -> (Engine, spg_sql::ast::Statement) {
    let mut eng = Engine::new();
    eng.execute(&format!(
        "CREATE TABLE v (id INT NOT NULL, e VECTOR({DIM}) NOT NULL)"
    ))
    .unwrap();
    for i in 0..rows {
        let lit = vec_text_literal(&vec_seed(i as u64 + 1, DIM));
        eng.execute(&format!("INSERT INTO v VALUES ({i}, {lit})"))
            .unwrap();
    }
    if with_hnsw {
        eng.execute("CREATE INDEX v_e_hnsw ON v USING HNSW (e)")
            .unwrap();
    }
    let stmt = eng
        .prepare("SELECT id FROM v ORDER BY e <-> $1 LIMIT 10")
        .unwrap();
    (eng, stmt)
}

#[test]
#[ignore]
fn perf_stages_knn_p50_fullscan() {
    run_stage_bench(false, "full-scan (no HNSW)", ROWS);
}

#[test]
#[ignore]
fn perf_stages_knn_p50_hnsw() {
    run_stage_bench(true, "HNSW-indexed", ROWS);
}

#[test]
#[ignore]
fn perf_stages_knn_p50_hnsw_100k() {
    run_stage_bench(true, "HNSW-indexed (100K rows)", ROWS_LARGE);
}

fn run_stage_bench(with_hnsw: bool, label: &str, rows: usize) {
    let (mut eng, stmt) = setup_engine(with_hnsw, rows);

    // Pre-build all query vectors so seeding doesn't leak into timing.
    let queries: Vec<Vec<f32>> = (0..(N + WARMUP))
        .map(|i| vec_seed(0xBEEF + i as u64, DIM))
        .collect();
    let queries_text: Vec<String> = queries.iter().map(|q| vec_text_literal(q)).collect();

    // ── Stage A: simple-query SQL build + execute (parse + eval) ──
    let mut simple_total = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let sql = format!(
            "SELECT id FROM v ORDER BY e <-> {} LIMIT 10",
            queries_text[i]
        );
        let t0 = Instant::now();
        let r = eng.execute(&sql).unwrap();
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(r);
        if i >= WARMUP {
            simple_total.push(dt);
        }
    }

    // ── Stage B: prepared (param decode + AST clone + substitute + eval) ──
    let mut prep_total = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let q = queries[i].clone();
        let t0 = Instant::now();
        let r = eng
            .execute_prepared(stmt.clone(), &[Value::Vector(q)])
            .unwrap();
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(r);
        if i >= WARMUP {
            prep_total.push(dt);
        }
    }

    // ── Stage C: text-format param decode only ──
    let mut decode = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let s = &queries_text[i];
        let t0 = Instant::now();
        let v = parse_vec_text(s);
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(v);
        if i >= WARMUP {
            decode.push(dt);
        }
    }

    // ── Stage D: AST clone only ──
    let mut clone_t = Vec::with_capacity(N);
    for _ in 0..(N + WARMUP) {
        let t0 = Instant::now();
        let s2 = stmt.clone();
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(s2);
        clone_t.push(dt);
    }
    let clone_t = clone_t.split_off(WARMUP);

    // ── Stage E: SQL parse only (no exec) ──
    let mut parse = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let sql = format!(
            "SELECT id FROM v ORDER BY e <-> {} LIMIT 10",
            queries_text[i]
        );
        let t0 = Instant::now();
        let s2 = eng.prepare(&sql).unwrap();
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(s2);
        if i >= WARMUP {
            parse.push(dt);
        }
    }

    let report = |name: &str, mut v: Vec<u128>| {
        let p50 = pct(&mut v.clone(), 0.50);
        let p90 = pct(&mut v.clone(), 0.90);
        let p99 = pct(&mut v, 0.99);
        println!(
            "  {:<28} p50={:>8} ns  p90={:>8} ns  p99={:>8} ns",
            name, p50, p90, p99
        );
    };

    // ── Stage F: Vec<bool> allocation (proxy for `visited` cost) ──
    let mut vec_alloc = Vec::with_capacity(N);
    for _ in 0..(N + WARMUP) {
        let t0 = Instant::now();
        let v: Vec<bool> = std::hint::black_box(alloc::vec![false; rows]);
        let dt = t0.elapsed().as_nanos();
        std::hint::black_box(v);
        vec_alloc.push(dt);
    }
    let vec_alloc = vec_alloc.split_off(WARMUP);

    println!();
    println!(
        "── v6.1.2 engine-level kNN stage timings  [{label}]  (dim={DIM}, rows={rows}, N={N}) ──"
    );
    report("Simple   (parse+eval)", simple_total);
    report("Prepared (eval only) ", prep_total);
    report("  └ param decode    ", decode);
    report("  └ AST clone       ", clone_t);
    report("  └ SQL parse only  ", parse);
    report("  └ vec![false; n]  ", vec_alloc);
    println!();
}

fn parse_vec_text(s: &str) -> Vec<f32> {
    let inner = &s[1..s.len() - 1];
    inner
        .split(',')
        .map(|t| t.trim().parse::<f32>().unwrap())
        .collect()
}
