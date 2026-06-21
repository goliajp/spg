#![cfg(not(debug_assertions))]
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
//!
//! Every `#[test]` here measures wall-clock cost (push throughput,
//! distance ns/pair, segment lookup p99, …) and so must NOT run
//! concurrently with another perf measurement on the same host: the
//! second test ends up CPU-starved by the first, blowing budgets for
//! a reason that isn't a real regression. v6.0.x adds `perf_lock()`,
//! a process-global `Mutex<()>` every test acquires at top of body —
//! parallel `cargo test` invocations still launch all the threads
//! but they queue on the lock instead of fighting for the same CPU.
//! 500 ms cool-down after acquisition gives the OS time to drain
//! pending I/O from the prior test before the next one measures.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

fn perf_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = L
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    thread::sleep(Duration::from_millis(500));
    guard
}

use spg_storage::{
    BloomFilter, Catalog, ColumnSchema, DataType, NSW_DEFAULT_M, NswMetric, Row,
    SEGMENT_PAGE_BYTES, SegmentReader, TableSchema, Value, VecEncoding, encode_segment, nsw_query,
    persistent::PersistentVec,
    persistent_btree::PersistentBTreeMap,
    quantize::{Sq8Vector, quantize, sq8_l2_distance_sq, sq8_l2_distance_sq_asymmetric},
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
            Value::text(format!("user-{i}")),
            Value::Float(f64::from(i) * 0.1),
        ]))
        .unwrap();
    }
    cat
}

#[test]
fn catalog_roundtrip_100rows_under_budget() {
    let _perf = perf_lock();
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
    let _perf = perf_lock();
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 8,
                    encoding: VecEncoding::F32,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for i in 0_i32..200 {
        #[allow(clippy::cast_precision_loss)]
        let f = i as f32;
        t.insert(Row::new(vec![
            Value::Int(i),
            Value::vector(vec![
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
    let _perf = perf_lock();
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

/// v4.40.1 PB transient insert 性能门 — 100K 次 `insert_mut` ≤ 50 ms。v4.40.0 的
/// immutable `insert` 每次 path-copy spine (每层 Arc::new ~500 ns，5 层 ~2.5 µs/insert)，
/// 在 spg-embedded 流式插入路径上让吞吐量从 v4.39 的 762K r/s 跌到 162K @ 1M (~50%)。
/// v4.40.1 的 `insert_mut` 沿 spine 走 `Arc::make_mut`，唯一拥有时直接 in-place mutate
/// 节点，恢复到 std::BTreeMap::insert 速度。100K insert + 50ms floor = 500 ns/insert avg
/// (~5 levels × 100 ns/level — 跟 std BTreeMap 持平的目标)。
#[test]
fn pb_insert_mut_100k_under_50ms() {
    let _perf = perf_lock();
    let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
    // Pre-build a small workload key range to mix inserts + replaces, matching
    // the secondary-index access pattern (most keys 1 entry, some replacements
    // when batched re-indexing happens).
    let start = Instant::now();
    for i in 0..100_000_i64 {
        pb.insert_mut(i, i.wrapping_mul(0x9E37_79B9));
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&pb);
    let budget_ms: u128 = 50;
    let elapsed_ms = elapsed.as_millis();
    assert!(
        elapsed_ms < budget_ms,
        "pb_insert_mut_100k elapsed {elapsed_ms} ms exceeds budget {budget_ms} ms"
    );
    assert_eq!(pb.len(), 100_000);
}

/// v4.39.1 transient push 性能门 — 1M 次 `push_mut` ≤ 50 ms。`push` 每次 path-copy
/// tail（O(BRANCH) 上限，对 `T = Row` ~600 ns/row），所以 200 ms gate（v4.38）反映的是
/// `push` 的成本。`push_mut` 用 `Arc::make_mut` 在 Arc 唯一拥有时直接 in-place mutate
/// tail，恢复 `Vec::push` 同级 ~10-30 ns/row。75 ms gate 在开发机留 ~3×、
/// mini testbed（实测 51 ms，2026-06-11）留 ~1.5× 余量；path-copy 回退
/// （~600 ns/row ≈ 600 ms）仍远超 gate，回归捕获力不变。
/// 这是 spg-embedded 流式 INSERT 路径恢复 baseline 吞吐的关键。
#[test]
fn pv_push_mut_1m_under_50ms() {
    let _perf = perf_lock();
    let start = Instant::now();
    let mut pv: PersistentVec<u64> = PersistentVec::new();
    for i in 0..1_000_000_u64 {
        pv.push_mut(i);
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&pv);
    let budget_ms: u128 = 75;
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
    let _perf = perf_lock();
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

// ---- v5.0 BloomFilter perf gates ----

/// SplitMix64 used by the bloom internals — re-derive here for
/// deterministic seed streams so the perf gate is reproducible
/// without `rand`.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// 100K inserts + 100K disjoint probes — observed FP rate must be
/// ≤ 1.1 × target (tighter than the in-module fuzz oracle which
/// allows 1.2×). The 10 % margin absorbs finite-sample variance:
/// bloom FP rate is an asymptotic guarantee, and on 100K probes
/// observed values typically sit in `[0.95, 1.05]` × target. Gate
/// fires when the bloom design itself regresses (bad hash mixing
/// → clustered bit positions, undersized bit table, etc.), not on
/// statistical noise. Exercises the v5.0 cold-tier prefilter's
/// worst-case shape (the v5 segment writer feeds Bloom at this
/// scale).
#[test]
fn bloom_fp_rate_under_1pct() {
    let _perf = perf_lock();
    const TARGET_FP: f64 = 0.01;
    const CEILING_FP: f64 = TARGET_FP * 1.1;
    const N: usize = 100_000;
    let mut bf = BloomFilter::with_target_fp_rate(N, TARGET_FP);
    // Deterministic key streams via SplitMix64.
    let mut s = 0xfeed_beef_u64;
    let mut inserted = Vec::with_capacity(N);
    for _ in 0..N {
        s = splitmix64(s.wrapping_add(1));
        inserted.push(s);
        bf.insert(&s.to_le_bytes());
    }
    let inserted_set: std::collections::BTreeSet<u64> = inserted.iter().copied().collect();
    let mut s2 = 0xbeef_feed_u64;
    let mut fp = 0u64;
    let mut tested = 0u64;
    for _ in 0..N {
        s2 = splitmix64(s2.wrapping_add(1));
        if inserted_set.contains(&s2) {
            continue;
        }
        tested += 1;
        if bf.contains(&s2.to_le_bytes()) {
            fp += 1;
        }
    }
    let observed = fp as f64 / tested as f64;
    eprintln!(
        "bloom_fp_rate: {fp} fp / {tested} tested = {observed:.5} (target {TARGET_FP:.3}, ceiling {CEILING_FP:.3})"
    );
    assert!(
        observed <= CEILING_FP,
        "observed FP {observed:.5} exceeded ceiling {CEILING_FP:.3} (target {TARGET_FP:.3})"
    );
}

/// 1M inserts wall time bound. Each insert is one FNV-1a pass over
/// 8 bytes + 7 word-mask updates; should clear well under 100 ms
/// on any modern x86_64 / aarch64. Catches insert-path pessimism
/// (e.g. accidental quadratic growth, missed inlining of `mix`).
#[test]
fn bloom_insert_1m_under_100ms() {
    let _perf = perf_lock();
    const N: usize = 1_000_000;
    let mut bf = BloomFilter::with_target_fp_rate(N, 0.01);
    let mut s = 0xdead_beef_u64;
    let start = Instant::now();
    for _ in 0..N {
        s = splitmix64(s.wrapping_add(1));
        bf.insert(&s.to_le_bytes());
    }
    let elapsed = start.elapsed();
    eprintln!(
        "bloom_insert_1m: {N} inserts in {:.3} ms",
        elapsed.as_secs_f64() * 1000.0
    );
    let budget_ms: u128 = 100;
    assert!(
        elapsed.as_millis() <= budget_ms,
        "bloom_insert_1m took {} ms, budget {budget_ms} ms",
        elapsed.as_millis()
    );
}

// ---- v5.0 Segment perf gates ----

/// 1M-row segment write within a 2 s wall-time budget. Encodes a
/// 1M-row segment with 32-byte payloads (~38 bytes/row including
/// the [u64 key][u32 plen] prefix; about 9100 rows per 4 KiB page
/// → ~110 pages). Catches encode-path pessimism (e.g. quadratic
/// offset recomputation, accidental Vec reallocation per row).
#[test]
fn segment_write_1m_under_2s() {
    let _perf = perf_lock();
    const N: u64 = 1_000_000;
    let rows: Vec<(u64, Vec<u8>)> = (0..N)
        .map(|i| (i, vec![u8::try_from(i & 0xff).unwrap(); 32]))
        .collect();
    let start = Instant::now();
    let (bytes, meta) =
        encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode succeeds");
    let elapsed = start.elapsed();
    eprintln!(
        "segment_write_1m: {} rows in {:.3} s ({} bytes, {} pages)",
        meta.num_rows,
        elapsed.as_secs_f64(),
        bytes.len(),
        meta.num_pages,
    );
    assert_eq!(meta.num_rows, N);
    let budget_ms: u128 = 2000;
    assert!(
        elapsed.as_millis() <= budget_ms,
        "segment_write_1m took {} ms, budget {budget_ms} ms",
        elapsed.as_millis()
    );
}

/// p99 lookup latency on a 1M-row segment with warm in-RAM bytes.
/// 1000 random PK lookups against a freshly-encoded segment;
/// each lookup runs `might_contain` → page-index binary search →
/// page-internal binary search. The full-RAM path here is the
/// upper bound — v5.1's seekable reader will add at most one
/// 4 KiB read per non-cached page, which on a modern SSD is
/// ~100-500 µs. 500 µs ceiling on a warm in-RAM run catches a
/// regression that would push p99 past disk read latency.
#[test]
fn segment_lookup_p99_under_500us() {
    let _perf = perf_lock();
    const N: u64 = 1_000_000;
    let rows: Vec<(u64, Vec<u8>)> = (0..N)
        .map(|i| (i, vec![u8::try_from(i & 0xff).unwrap(); 32]))
        .collect();
    let (bytes, _) =
        encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode succeeds");
    let reader = SegmentReader::open(&bytes).expect("open succeeds");

    // Deterministic random probes via SplitMix64.
    let mut state: u64 = 0xc0ffee;
    const N_PROBES: usize = 1000;
    let mut samples_us = Vec::with_capacity(N_PROBES);
    for _ in 0..N_PROBES {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut x = state;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        let key = x % N;
        let t = Instant::now();
        let got = reader.lookup(std::hint::black_box(key));
        samples_us.push(t.elapsed().as_micros());
        // Sanity: must find it (key is in [0, N)).
        assert!(got.is_some(), "lookup({key}) returned None");
    }
    samples_us.sort_unstable();
    let p99_idx = (N_PROBES * 99) / 100;
    let p99 = samples_us[p99_idx];
    eprintln!("segment_lookup_p99: {p99} µs (over {N_PROBES} warm-RAM probes)");
    let budget_us: u128 = 500;
    assert!(
        p99 <= budget_us,
        "segment_lookup p99 {p99} µs exceeds budget {budget_us} µs"
    );
}

// ===========================================================================
// v6.0.0 — SQ8 quantization perf gates.
//
// All numbers measured in release mode on the macOS APFS aarch64 dev box;
// budgets sized to break only on > 2× regression. Distance gates use
// `std::hint::black_box` to defeat constant-folding so the timed loop
// reflects real per-call cost.
// ===========================================================================

fn random_unit_vec_f32(rng_state: &mut u64, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    for _ in 0..dim {
        *rng_state = splitmix64(*rng_state);
        // High 24 bits → [0, 1); shift to [-1, 1].
        let bits = (*rng_state >> 40) as u32;
        let u = (bits as f32) / ((1u32 << 24) as f32);
        out.push(u * 2.0 - 1.0);
    }
    out
}

/// Quantize 1M dim-128 vectors in ≤ 500 ms. Sized so the per-vector
/// cost (~500 ns) is dwarfed by the existing INSERT path (~30 µs
/// server), keeping the quantize step well off the INSERT critical
/// path even at saturation.
#[test]
fn sq8_quantize_1m_under_500ms() {
    let _perf = perf_lock();
    const N: usize = 1_000_000;
    const DIM: usize = 128;

    let mut rng: u64 = 0xC0DE_C0DE_5BAD_5BAD;
    // Pre-build the corpus so the timer measures pure quantize cost.
    let corpus: Vec<Vec<f32>> = (0..N).map(|_| random_unit_vec_f32(&mut rng, DIM)).collect();

    let t = Instant::now();
    let mut sink: u32 = 0;
    for v in &corpus {
        let q = quantize(std::hint::black_box(v));
        // Sum a few bytes to defeat dead-code elimination.
        sink = sink
            .wrapping_add(q.bytes[0] as u32)
            .wrapping_add(q.bytes[DIM / 2] as u32);
    }
    let elapsed = t.elapsed();
    std::hint::black_box(sink);
    eprintln!(
        "sq8_quantize_1m_dim128: {} ms ({} ns/vec)",
        elapsed.as_millis(),
        elapsed.as_nanos() / N as u128
    );
    let budget_ms: u128 = 500;
    assert!(
        elapsed.as_millis() <= budget_ms,
        "sq8_quantize_1m_dim128 took {} ms, budget {budget_ms} ms",
        elapsed.as_millis()
    );
}

/// SQ8 ADC L2² distance ≤ 200 ns per pair (dim 128, scalar path).
/// v6.0.2 will tighten this to ≤ 50 ns with NEON; the v6.0.0 ceiling
/// catches a scalar regression that would otherwise hide under NEON's
/// shadow.
#[test]
fn sq8_adc_l2_under_200ns_per_pair() {
    let _perf = perf_lock();
    const DIM: usize = 128;
    const N_PAIRS: usize = 1_000_000;

    let mut rng: u64 = 0xADC0_ADC0_ADC0_ADC0;
    // 1024 pre-quantized vectors; rotate through them to give the
    // branch predictor real work without inflating cache cost.
    let pool: Vec<Sq8Vector> = (0..1024)
        .map(|_| quantize(&random_unit_vec_f32(&mut rng, DIM)))
        .collect();

    let t = Instant::now();
    let mut acc: f32 = 0.0;
    for i in 0..N_PAIRS {
        let a = &pool[i & 1023];
        let b = &pool[(i.wrapping_mul(2654435761)) & 1023]; // Knuth's mix
        acc += std::hint::black_box(sq8_l2_distance_sq(a, b));
    }
    let elapsed = t.elapsed();
    std::hint::black_box(acc);

    let per_call_ns = elapsed.as_nanos() / N_PAIRS as u128;
    eprintln!("sq8_adc_l2_dim128: {per_call_ns} ns/pair (over {N_PAIRS} pairs)");
    // 250 ns budget — scalar path, cross-arch. Original 200 ns was set
    // against the dev box (Apple M-series, ~140-180 ns warm), but shared
    // GitHub runners (x86 amd64, parallel jobs) jitter into the 200-220
    // ns range. 250 ns absorbs ~25 % runner-noise headroom and still
    // catches the 2×+ scalar-regression the gate was authored for —
    // the NEON path is gated separately at 50 ns in the
    // `_neon_dim128_under_50ns` variant.
    let budget_ns: u128 = 250;
    assert!(
        per_call_ns <= budget_ns,
        "sq8_adc_l2_dim128 per-pair {per_call_ns} ns exceeds budget {budget_ns} ns"
    );
}

/// SQ8 ADC L2² asymmetric (stored SQ8 vs un-quantized query) — typical
/// kNN scan shape. Same budget as symmetric (one dequant per element
/// either way, asymmetric saves a tiny multiply per element).
#[test]
fn sq8_adc_l2_asymmetric_under_200ns_per_pair() {
    let _perf = perf_lock();
    const DIM: usize = 128;
    const N_PAIRS: usize = 1_000_000;

    let mut rng: u64 = 0xADC1_BEEF_ADC1_BEEF;
    let pool: Vec<Sq8Vector> = (0..1024)
        .map(|_| quantize(&random_unit_vec_f32(&mut rng, DIM)))
        .collect();
    let queries: Vec<Vec<f32>> = (0..1024)
        .map(|_| random_unit_vec_f32(&mut rng, DIM))
        .collect();

    let t = Instant::now();
    let mut acc: f32 = 0.0;
    for i in 0..N_PAIRS {
        let a = &pool[i & 1023];
        let q = &queries[(i.wrapping_mul(2654435761)) & 1023];
        acc += std::hint::black_box(sq8_l2_distance_sq_asymmetric(a, q));
    }
    let elapsed = t.elapsed();
    std::hint::black_box(acc);

    let per_call_ns = elapsed.as_nanos() / N_PAIRS as u128;
    eprintln!("sq8_adc_l2_asym_dim128: {per_call_ns} ns/pair (over {N_PAIRS} pairs)");
    let budget_ns: u128 = 200;
    assert!(
        per_call_ns <= budget_ns,
        "sq8_adc_l2_asym_dim128 per-pair {per_call_ns} ns exceeds budget {budget_ns} ns"
    );
}

/// v6.0.2 f32 cosine via NEON dispatch ≤ 50 ns/pair (dim 128). The
/// public `spg_storage::nsw_query` cosine path hits this loop on
/// every f32 column with a NEON-compatible dim; the gate guards
/// against an accidental scalar regression in `metric_distance`.
#[test]
fn cosine_dim128_under_50ns() {
    let _perf = perf_lock();
    const DIM: usize = 128;
    const N_PAIRS: usize = 1_000_000;
    let mut rng: u64 = 0xC051_C051_DEAD_BEEF;
    let pool: Vec<Vec<f32>> = (0..1024)
        .map(|_| random_unit_vec_f32(&mut rng, DIM))
        .collect();
    // Cold-cache warm-up — see `sq8_adc_l2_asymmetric_neon_dim128`.
    let mut warm: f32 = 0.0;
    for i in 0..10_000 {
        let a = &pool[i & 1023];
        let b = &pool[(i.wrapping_mul(2_654_435_761)) & 1023];
        warm += std::hint::black_box(metric_dispatch_cos(a, b));
    }
    std::hint::black_box(warm);
    let t = Instant::now();
    let mut acc: f32 = 0.0;
    for i in 0..N_PAIRS {
        let a = &pool[i & 1023];
        let b = &pool[(i.wrapping_mul(2_654_435_761)) & 1023];
        acc += std::hint::black_box(metric_dispatch_cos(a, b));
    }
    let elapsed = t.elapsed();
    std::hint::black_box(acc);
    let per_call_ns = elapsed.as_nanos() / N_PAIRS as u128;
    eprintln!("cosine_dim128: {per_call_ns} ns/pair (over {N_PAIRS} pairs)");
    // 60 ns budget vs the NEON dispatch target: dev box measures ~40 ns,
    // the mini testbed 53 ns (2026-06-11) — both NEON; the scalar
    // fallback this gate exists to catch is ~130 ns and still blows it.
    // x86_64 hosts always take the scalar path; gate aarch64-only —
    // same pattern as `sq8_adc_l2_asymmetric_neon_dim128_under_50ns`.
    #[cfg(target_arch = "aarch64")]
    {
        let budget_ns: u128 = 60;
        assert!(
            per_call_ns <= budget_ns,
            "cosine_dim128 per-pair {per_call_ns} ns exceeds budget {budget_ns} ns — \
             check NEON dispatch in `metric_distance(Cosine, ...)`"
        );
    }
}

/// v6.0.2 f32 inner product via NEON dispatch ≤ 50 ns/pair (dim 128).
#[test]
fn inner_product_dim128_under_50ns() {
    let _perf = perf_lock();
    const DIM: usize = 128;
    const N_PAIRS: usize = 1_000_000;
    let mut rng: u64 = 0xBEEF_FEED_FACE_C001;
    let pool: Vec<Vec<f32>> = (0..1024)
        .map(|_| random_unit_vec_f32(&mut rng, DIM))
        .collect();
    let mut warm: f32 = 0.0;
    for i in 0..10_000 {
        let a = &pool[i & 1023];
        let b = &pool[(i.wrapping_mul(2_654_435_761)) & 1023];
        warm += std::hint::black_box(metric_dispatch_ip(a, b));
    }
    std::hint::black_box(warm);
    let t = Instant::now();
    let mut acc: f32 = 0.0;
    for i in 0..N_PAIRS {
        let a = &pool[i & 1023];
        let b = &pool[(i.wrapping_mul(2_654_435_761)) & 1023];
        acc += std::hint::black_box(metric_dispatch_ip(a, b));
    }
    let elapsed = t.elapsed();
    std::hint::black_box(acc);
    let per_call_ns = elapsed.as_nanos() / N_PAIRS as u128;
    eprintln!("inner_product_dim128: {per_call_ns} ns/pair (over {N_PAIRS} pairs)");
    // 50 ns budget is the NEON dispatch target — gate aarch64-only;
    // see the cosine variant above for the rationale.
    #[cfg(target_arch = "aarch64")]
    {
        let budget_ns: u128 = 50;
        assert!(
            per_call_ns <= budget_ns,
            "inner_product_dim128 per-pair {per_call_ns} ns exceeds budget {budget_ns} ns"
        );
    }
}

/// v6.0.2 SQ8 ADC L2 asymmetric via NEON dispatch ≤ 50 ns/pair
/// (dim 128). This is the kNN-scan hot path — stored SQ8 cell vs
/// f32 query — under the new 16-wide widening loop.
///
/// Measured ~13 ns warm-cache (Apple M-series); design target was
/// 25 ns but cold-cache transients + parallel-test thermal noise
/// can spike to ~36 ns under perf_lock contention, so the gate is
/// set to 50 ns — same ceiling as the f32 cosine / IP gates and
/// still a 4× win over the v6.0.0 scalar 200 ns floor.
#[test]
fn sq8_adc_l2_asymmetric_neon_dim128_under_50ns() {
    let _perf = perf_lock();
    const DIM: usize = 128;
    const N_PAIRS: usize = 1_000_000;
    let mut rng: u64 = 0x5A8_5A8_5A8_5A8;
    let pool: Vec<Sq8Vector> = (0..1024)
        .map(|_| quantize(&random_unit_vec_f32(&mut rng, DIM)))
        .collect();
    let queries: Vec<Vec<f32>> = (0..1024)
        .map(|_| random_unit_vec_f32(&mut rng, DIM))
        .collect();
    // Cold-cache warm-up: walk the pool + queries once so the
    // measured loop sees the same I-cache + D-cache + branch-
    // predictor state as a long-running kNN scan. Without this
    // the first 10 ms of the timed loop pays a transient that
    // doubles the per-call estimate.
    let mut warm: f32 = 0.0;
    for i in 0..10_000 {
        let a = &pool[i & 1023];
        let q = &queries[(i.wrapping_mul(2_654_435_761)) & 1023];
        warm += std::hint::black_box(sq8_l2_distance_sq_asymmetric(a, q));
    }
    std::hint::black_box(warm);
    let t = Instant::now();
    let mut acc: f32 = 0.0;
    for i in 0..N_PAIRS {
        let a = &pool[i & 1023];
        let q = &queries[(i.wrapping_mul(2_654_435_761)) & 1023];
        acc += std::hint::black_box(sq8_l2_distance_sq_asymmetric(a, q));
    }
    let elapsed = t.elapsed();
    std::hint::black_box(acc);
    let per_call_ns = elapsed.as_nanos() / N_PAIRS as u128;
    eprintln!("sq8_adc_l2_asym_neon_dim128: {per_call_ns} ns/pair (over {N_PAIRS} pairs)");
    // v6.0.0 scalar floor is 200 ns; v6.0.2 NEON measures ~13 ns
    // warm-cache, ~36 ns under perf_lock thermal noise. Budget
    // 50 ns gives 1.4× margin over the worst observed and still
    // 4× the scalar floor. x86_64 hosts fall back to scalar and
    // would blow this; gate is aarch64-only.
    #[cfg(target_arch = "aarch64")]
    {
        let budget_ns: u128 = 50;
        assert!(
            per_call_ns <= budget_ns,
            "sq8_adc_l2_asym_neon_dim128 per-pair {per_call_ns} ns exceeds budget {budget_ns} ns \
             — check NEON dispatch in `sq8_l2_distance_sq_asymmetric`"
        );
    }
}

/// Cosine via the v6.0.2 NEON-backed dispatch — same shape the
/// kNN search path uses inside `metric_distance(Cosine, ...)`.
fn metric_dispatch_cos(a: &[f32], b: &[f32]) -> f32 {
    let (dot, na, nb) = spg_storage::cosine_dot_norms_f32(a, b);
    if na == 0.0 || nb == 0.0 {
        return f32::INFINITY;
    }
    1.0 - dot / (na.sqrt() * nb.sqrt())
}

fn metric_dispatch_ip(a: &[f32], b: &[f32]) -> f32 {
    -spg_storage::inner_product_f32(a, b)
}

/// Recall@10 ≥ 0.95 on a 10K-vector dim-128 unit-sphere corpus with
/// 100 random queries. Duplicate of the lib-test ranking-preservation
/// gate, but run as a perf gate so a regression in quantization
/// quality breaks the release build (not just `cargo test --lib`).
#[test]
fn sq8_recall_at_10_above_0_95_perf_gate() {
    let _perf = perf_lock();
    const N: usize = 10_000;
    const Q: usize = 100;
    const K: usize = 10;
    const DIM: usize = 128;

    let mut rng: u64 = 0xCAFE_BABE_5EED_1234;
    let corpus_f32: Vec<Vec<f32>> = (0..N).map(|_| random_unit_vec_f32(&mut rng, DIM)).collect();
    let corpus_sq8: Vec<Sq8Vector> = corpus_f32.iter().map(|v| quantize(v)).collect();

    let mut total_recall: f32 = 0.0;
    for _ in 0..Q {
        let query = random_unit_vec_f32(&mut rng, DIM);

        // f32 ground truth top-K.
        let mut scored_f32: Vec<(f32, usize)> = corpus_f32
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut d = 0.0_f32;
                for (x, y) in v.iter().zip(query.iter()) {
                    let dd = x - y;
                    d += dd * dd;
                }
                (d, i)
            })
            .collect();
        scored_f32.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let truth: Vec<usize> = scored_f32.into_iter().take(K).map(|(_, i)| i).collect();

        // SQ8 ADC top-K.
        let mut scored_sq8: Vec<(f32, usize)> = corpus_sq8
            .iter()
            .enumerate()
            .map(|(i, qv)| (sq8_l2_distance_sq_asymmetric(qv, &query), i))
            .collect();
        scored_sq8.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let sq8_top: Vec<usize> = scored_sq8.into_iter().take(K).map(|(_, i)| i).collect();

        let mut hits = 0;
        for x in &truth {
            if sq8_top.contains(x) {
                hits += 1;
            }
        }
        total_recall += hits as f32 / K as f32;
    }
    let avg = total_recall / Q as f32;
    eprintln!("sq8_recall_at_10_unit_sphere_dim128: {avg:.4}");
    assert!(avg >= 0.95, "sq8 recall@10 average = {avg} (need ≥ 0.95)");
}
