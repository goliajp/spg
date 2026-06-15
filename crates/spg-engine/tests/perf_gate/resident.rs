//! v7.33 (C1) — the resident gate. The ceiling-first design
//! (.claude/notes/v7.31-memory-design.md, "桶 E 放大系数 k"): the
//! engine's resident footprint is ~k × the raw dataset, and k must
//! stay bounded — the R30 prod incident was an 11× resident blow-up.
//! This loads a known quantity of fat rows and asserts the engine's
//! own `memory_stats()` resident accounting stays within a bounded
//! multiple of the bytes actually inserted, so a representation
//! regression (fat enum variants, double-stored bodies) trips it.
//!
//! Companion to never_die (peak under a budget) and round26_mem (peak
//! bounded by LIMIT). Deterministic accounting, fast tier / CI.

use crate::perf_lock;
use spg_engine::Engine;

const ROWS: usize = 2_000;
const BODY_BYTES: usize = 4 * 1024; // 4 KiB bodies → 8 MiB of raw text
/// Measured k on the current representation is ~1.x (resident ≈ the
/// encoded bytes plus per-row Value overhead). 4x ceiling locks "k does
/// not regress" with headroom, well under the 11x R30 blow-up.
const K_CEILING: u64 = 4;

#[test]
fn resident_footprint_is_a_bounded_multiple_of_data() {
    let _g = perf_lock();
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT, body TEXT)")
        .unwrap();

    let mut raw_bytes: u64 = 0;
    let mut i = 0usize;
    while i < ROWS {
        let mut stmt = String::with_capacity(50 * BODY_BYTES);
        stmt.push_str("INSERT INTO t VALUES ");
        for k in 0..50 {
            let id = i + k + 1;
            if k > 0 {
                stmt.push(',');
            }
            let body = format!("{id:08}{}", "x".repeat(BODY_BYTES - 8));
            raw_bytes += 8 + body.len() as u64; // id (8B) + body bytes
            stmt.push_str(&format!("({id},'{body}')"));
        }
        eng.execute(&stmt).unwrap();
        i += 50;
    }

    let stats = eng.memory_stats();
    let resident = stats.total_approx_resident_bytes;
    let k = resident as f64 / raw_bytes as f64;
    println!("RESIDENT raw={raw_bytes} resident={resident} k={k:.2}");

    assert!(
        resident <= raw_bytes * K_CEILING,
        "resident {resident} bytes is {k:.1}x the {raw_bytes} bytes of data \
         (ceiling {K_CEILING}x) — the representation amplification regressed \
         toward the R30 11x blow-up"
    );
}
