//! v7.39 (round 485) — profiling target for the row-source itself.
//!
//! `SELECT DISTINCT g FROM h ORDER BY g` is the panel's worst read shape
//! (0.86x) and it carries no WHERE, so none of rounds 479-484 could reach
//! it: whatever it spends, it spends getting rows out of the table and
//! deduplicating them. This probe runs that shape alone so a profile
//! attributes the row source rather than the predicate.
//!
//! `SPG_PROF_SHAPE=scan` swaps in a bare projection to separate the row
//! source from the DISTINCT set work.

#![allow(clippy::doc_markdown)]

use spg_engine::Engine;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn main() {
    let n: i64 = 50_000;
    let start = std::time::Instant::now();
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .expect("create");
    eng.execute("CREATE INDEX h_v_idx ON h (v)").expect("idx v");
    eng.execute("CREATE INDEX h_g_idx ON h (g)").expect("idx g");
    for i in 1..=n {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h VALUES ({i}, {g}, {v})"))
            .expect("seed");
    }
    let seeded = std::time::Instant::now();
    let sql = match std::env::var("SPG_PROF_SHAPE").as_deref() {
        Ok("scan") => "SELECT g FROM h",
        // The panel's `big_in`: a 50-literal IN list over the same table.
        Ok("big_in") => {
            "SELECT count(*) FROM h WHERE g IN (1,3,5,7,9,11,13,15,17,19,21,23,25,27,29,\
             31,33,35,37,39,41,43,45,47,49,51,53,55,57,59,61,63,65,67,69,71,73,75,77,79,\
             81,83,85,87,89,91,93,95,97,99)"
        }
        // Same row count, same column, a predicate the round-482 fast
        // path DOES cover — the contrast that says whether `big_in`'s
        // cost is the IN set or the machinery around it.
        Ok("eq") => "SELECT count(*) FROM h WHERE g = 5",
        _ => "SELECT DISTINCT g FROM h ORDER BY g",
    };
    let iters: usize = std::env::var("SPG_PROF_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        let _ = eng.execute(sql).expect("q");
    }
    // The profile covers the whole process, seeding included. Print both
    // legs so the reader can tell what share of the samples is the shape
    // under test rather than 50 000 INSERTs.
    eprintln!(
        "seed {:?} / {iters} x query {:?}",
        seeded.duration_since(start),
        t0.elapsed()
    );
}
