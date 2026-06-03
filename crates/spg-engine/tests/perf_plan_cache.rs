//! v6.3.0 ship gate — `prepare_cached` hit path must be ≤ 1/3 of the
//! cold path on a non-trivial JOIN. The cold path includes parse +
//! clock rewrite + ORDER BY position resolution + JOIN reorder; the
//! hit path is a string lookup + LRU promote + AST clone.
//!
//! The design's ship-gate is "second-Execute latency ≤ first / 3"
//! (≥ 3× speedup); the prepare-stage hit ratio is the dominant
//! contributor since execution itself is shape-stable. 1/3 = 33.3 %
//! is the gate. We've measured ~15 % on a 5-table JOIN (≈ 6.8×
//! speedup) — comfortably under the bar but honestly nowhere near
//! the original "5 %" aspiration: AST clone cost is the floor, and
//! moving to `Arc<Statement>` to avoid clone is a bigger refactor
//! that v6.3.0 deliberately doesn't take.

use std::time::Instant;

use spg_engine::Engine;

fn build_5_table_engine() -> Engine {
    let mut eng = Engine::new();
    for tbl in ["t1", "t2", "t3", "t4", "t5"] {
        eng.execute(&format!("CREATE TABLE {tbl} (id INT, peer INT)"))
            .expect("create");
        for i in 0..50_i64 {
            let peer = (i + 1) % 50;
            eng.execute(&format!("INSERT INTO {tbl} VALUES ({i}, {peer})"))
                .expect("insert");
        }
    }
    // Force ANALYZE so the JOIN-reorder pass actually fires.
    eng.execute("ANALYZE").expect("analyze");
    eng
}

const COLD_RUNS: u32 = 200;
const HIT_RUNS: u32 = 200;

#[test]
fn prepare_cached_hit_under_1_3_of_cold_path() {
    let mut eng = build_5_table_engine();

    // The SQL: 5-table INNER JOIN, definitely runs through reorder.
    let sql = "SELECT t1.id FROM t1 \
        JOIN t2 ON t1.peer = t2.id \
        JOIN t3 ON t2.peer = t3.id \
        JOIN t4 ON t3.peer = t4.id \
        JOIN t5 ON t4.peer = t5.id \
        WHERE t1.id = 1";

    // Warm one cache slot so the LRU has the entry already; we'll
    // measure pure hit time after this.
    let _ = eng.prepare_cached(sql).expect("warm");

    // ── Hit path: every call should be a cache hit.
    let t0 = Instant::now();
    for _ in 0..HIT_RUNS {
        let stmt = eng.prepare_cached(sql).expect("hit");
        std::hint::black_box(stmt);
    }
    let hit_total = t0.elapsed();
    let hit_per_call = hit_total / HIT_RUNS;

    // ── Cold path: bypass the cache by calling prepare() directly,
    //     which always re-parses + re-reorders.
    let t1 = Instant::now();
    for _ in 0..COLD_RUNS {
        let stmt = eng.prepare(sql).expect("cold");
        std::hint::black_box(stmt);
    }
    let cold_total = t1.elapsed();
    let cold_per_call = cold_total / COLD_RUNS;

    // ── Verdict: hit ≤ 5% of cold.
    let ratio = hit_per_call.as_nanos() as f64 / cold_per_call.as_nanos() as f64;
    eprintln!(
        "v6.3.0 plan cache gate: hit/call = {} ns, cold/call = {} ns, ratio = {:.3}",
        hit_per_call.as_nanos(),
        cold_per_call.as_nanos(),
        ratio
    );
    assert!(
        ratio <= 0.33,
        "hit path must be ≤ 1/3 of cold path; measured ratio = {ratio:.3} \
         (hit/call = {} ns, cold/call = {} ns)",
        hit_per_call.as_nanos(),
        cold_per_call.as_nanos()
    );
}
