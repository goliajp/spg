//! v7.39 (round 488) — does an aggregate query reach the single-table
//! SCAN path, and does round 487's projection binding fire there?
//!
//! The interleaved panel says round 487 costs `group_500k` 13 % and
//! `agg_500k` 9 %, both with separated spreads; a probe that adds a
//! never-called function to the same file moves them ~1 %, so it is not
//! code layout. That leaves "the shape reaches the changed code", which
//! contradicts reading the dispatch — so ask the counter.
//!
//!   cargo run --release --features perf-counters --example probe_agg_scan_path

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn main() {
    let n: i64 = 50_000;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    for i in 1..=n {
        eng.execute(&format!(
            "INSERT INTO h VALUES ({i}, {}, {})",
            i % 100,
            val_for(i)
        ))
        .unwrap();
    }
    println!("| query | scans entered | direct proj fires |");
    println!("|-------|--------------:|------------------:|");
    for sql in [
        "SELECT count(*), sum(v), avg(v)::float8 FROM h",
        "SELECT g, count(*), sum(v) FROM h GROUP BY g ORDER BY g",
        "SELECT DISTINCT g FROM h ORDER BY g",
        "SELECT g FROM h",
    ] {
        let base = (
            spg_engine::SCAN_PATH_ENTERED.load(Relaxed),
            spg_engine::PROJ_DIRECT_FIRE.load(Relaxed),
        );
        let _ = eng.execute(sql).expect("q");
        println!(
            "| {sql} | {} | {} |",
            spg_engine::SCAN_PATH_ENTERED.load(Relaxed) - base.0,
            spg_engine::PROJ_DIRECT_FIRE.load(Relaxed) - base.1,
        );
    }
}
