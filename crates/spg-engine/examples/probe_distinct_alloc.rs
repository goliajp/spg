//! v7.39 (round 485) — how many projected rows does `SELECT DISTINCT g`
//! build, and how many does it throw away?
//!
//! The round-485 profile of `distinct_proj` (the read panel's worst shape
//! at 0.87x) puts 21 % of all samples in malloc/free called from the scan
//! closure. The closure allocates one `Vec<Value>` per input row for the
//! projected row; under DISTINCT the row is discarded again a few
//! instructions later whenever it duplicates one already kept. This
//! counts both so the waste is a number before any code moves.
//!
//! Build with the counters on:
//!   cargo run --release --features perf-counters --example probe_distinct_alloc

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn main() {
    let n: i64 = 50_000;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .expect("create");
    for i in 1..=n {
        eng.execute(&format!(
            "INSERT INTO h VALUES ({i}, {}, {})",
            i % 100,
            val_for(i)
        ))
        .expect("seed");
    }
    println!("| query | rows built | dups dropped | waste |");
    println!("|-------|-----------:|-------------:|------:|");
    for (label, sql) in [
        ("distinct_proj", "SELECT DISTINCT g FROM h ORDER BY g"),
        ("distinct_wide", "SELECT DISTINCT v FROM h"),
        ("plain_proj", "SELECT g FROM h"),
    ] {
        let base = (
            spg_engine::PROJ_ROW_BUILT.load(Relaxed),
            spg_engine::DISTINCT_DUP_DROPPED.load(Relaxed),
        );
        let _ = eng.execute(sql).expect("q");
        let built = spg_engine::PROJ_ROW_BUILT.load(Relaxed) - base.0;
        let dropped = spg_engine::DISTINCT_DUP_DROPPED.load(Relaxed) - base.1;
        let pct = if built == 0 {
            0.0
        } else {
            100.0 * dropped as f64 / built as f64
        };
        println!("| {label} | {built} | {dropped} | {pct:.1}% |");
    }
}
