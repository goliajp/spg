//! v7.39 (round 478) — the profiling target for the round-478 gate.
//!
//! `SELECT count(*) FROM hi WHERE g = 5` over 50 000 rows, run long enough
//! for a sampler to see it. Round 478's decomposition put SPGE at 6.6 ns a
//! row with no predicate and 32.9 ns with one, whatever the type or
//! operator — so this loop is 80 % per-row predicate evaluation and the
//! profile should say where inside it.

#![allow(clippy::doc_markdown)]

use spg_engine::Engine;

fn main() {
    let n: i64 = 50_000;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE hi (id INT NOT NULL, g INT NOT NULL)")
        .expect("create");
    for i in 1..=n {
        eng.execute(&format!("INSERT INTO hi VALUES ({i}, {})", i % 1000))
            .expect("seed");
    }
    let iters: usize = std::env::var("SPG_PROF_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    for _ in 0..iters {
        let _ = eng.execute("SELECT count(*) FROM hi WHERE g = 5").expect("q");
    }
}
