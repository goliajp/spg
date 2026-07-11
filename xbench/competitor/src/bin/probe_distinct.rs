//! Phase-A isolation probes for the distinct_proj 4× loss
//! (`SELECT DISTINCT g FROM h ORDER BY g`, heavy.rs). Embedded-only;
//! min-of-N so scheduler noise doesn't inflate the per-stage deltas.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin probe_distinct`

#![allow(clippy::cast_precision_loss, clippy::uninlined_format_args)]

use std::time::Instant;

const N: i64 = 50_000;
const RUNS: usize = 15;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    eng.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    for i in 1..=N {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h VALUES ({i}, {g}, {v})"))
            .unwrap();
    }

    let probes: &[(&str, &str)] = &[
        ("bare_proj", "SELECT g FROM h"),
        ("distinct_only", "SELECT DISTINCT g FROM h"),
        ("distinct_order", "SELECT DISTINCT g FROM h ORDER BY g"),
        ("distinct_wide", "SELECT DISTINCT v FROM h"),
        ("distinct_two", "SELECT DISTINCT g, v FROM h"),
        ("proj_order", "SELECT g FROM h ORDER BY g"),
        ("agg_distinct", "SELECT count(DISTINCT g) FROM h"),
        ("group_as_distinct", "SELECT g FROM h GROUP BY g ORDER BY g"),
    ];
    println!("| probe | min ms | ns/row |");
    println!("|-------|-------:|-------:|");
    for (name, sql) in probes {
        for _ in 0..3 {
            eng.execute(sql).unwrap();
        }
        let mut best = f64::MAX;
        for _ in 0..RUNS {
            let t0 = Instant::now();
            eng.execute(sql).unwrap();
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            if ms < best {
                best = ms;
            }
        }
        println!(
            "| {:<17} | {:>7.3} | {:>6.1} |",
            name,
            best,
            best * 1e6 / N as f64
        );
    }
}
