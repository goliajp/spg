//! Phase-A isolation probes for the agg_500k 1.6× loss
//! (`SELECT count(*), sum(v), avg(v)::float8 FROM h5`, heavy.rs).
//! PG18 wins via parallel seq scan+agg (~17.9 ns/row effective);
//! SPGE is single-threaded at ~28.5 ns/row — find where those go.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin probe_agg500k`

#![allow(clippy::cast_precision_loss, clippy::uninlined_format_args)]

use std::time::Instant;

const N: i64 = 500_000;
const RUNS: usize = 11;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h5 (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    for i in 1..=N {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h5 VALUES ({i}, {g}, {v})"))
            .unwrap();
    }

    let probes: &[(&str, &str)] = &[
        ("count_only", "SELECT count(*) FROM h5"),
        ("sum_only", "SELECT sum(v) FROM h5"),
        ("avg_only", "SELECT avg(v)::float8 FROM h5"),
        ("sum_avg", "SELECT sum(v), avg(v)::float8 FROM h5"),
        ("count_sum", "SELECT count(*), sum(v) FROM h5"),
        ("full3", "SELECT count(*), sum(v), avg(v)::float8 FROM h5"),
        ("sum_g_v", "SELECT sum(g), sum(v) FROM h5"),
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
            "| {:<10} | {:>7.3} | {:>6.1} |",
            name,
            best,
            best * 1e6 / N as f64
        );
    }
}
