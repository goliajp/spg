//! v7.39 (round 484) — profiling target for the LIKE step.
//!
//! `SELECT count(*) FROM ht WHERE s LIKE '%_05%'` over 50 000 rows of
//! `user_NNNN`. After rounds 482-483 an equality predicate over the same
//! table costs 14.8 ns a row and this one costs 30.8, so the LIKE step is
//! worth about 16 ns — the largest single item left in the panel's
//! predicate work, and the one no round has touched.

#![allow(clippy::doc_markdown)]

use spg_engine::Engine;

fn main() {
    let n: i64 = 50_000;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE ht (id INT NOT NULL, s TEXT NOT NULL)")
        .expect("create");
    for i in 1..=n {
        let tag = i % 1000;
        eng.execute(&format!("INSERT INTO ht VALUES ({i}, 'user_{tag:04}')"))
            .expect("seed");
    }
    let iters: usize = std::env::var("SPG_PROF_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    for _ in 0..iters {
        let _ = eng
            .execute("SELECT count(*) FROM ht WHERE s LIKE '%_05%'")
            .expect("q");
    }
}
