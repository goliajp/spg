//! Phase 3 pre-Phase-A gate (round 213): does SPG's O(n) EXCLUDE
//! enforcement scale quadratically with table size? Insert N
//! non-overlapping int4ranges into an EXCLUDE table and time it at
//! several N. O(n)/insert → total O(N^2): doubling N ~4x the time.
//! A real GiST index (Phase 3) would make each insert O(log n) →
//! total O(N log N): doubling N ~2.1x. This decides whether the
//! index epic is justified by a measured gap.
//!
//! Run (release, on mini): cargo run --release --example probe_exclude_scaling

use spg_engine::Engine;
use std::time::Instant;

fn bench(n: usize) -> f64 {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bk (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        // Non-overlapping [2i, 2i+1) — every insert must scan all prior
        // rows and find no overlap (the worst case for the O(n) path).
        let lo = 2 * i;
        let hi = 2 * i + 1;
        e.execute(&format!("INSERT INTO bk VALUES ('[{lo},{hi})')"))
            .unwrap();
    }
    start.elapsed().as_secs_f64()
}

fn main() {
    println!("{:>8}  {:>10}  {:>12}  {:>8}", "N", "total_s", "us/insert", "ratio");
    let mut prev: Option<(usize, f64)> = None;
    for &n in &[1000usize, 2000, 4000, 8000] {
        let t = bench(n);
        let us = t / n as f64 * 1e6;
        let ratio = prev.map(|(_, pt)| t / pt).unwrap_or(0.0);
        println!("{n:>8}  {t:>10.4}  {us:>12.2}  {ratio:>8.2}");
        prev = Some((n, t));
    }
    println!("\nO(N^2) ⇒ ratio ~4.0 (each N-doubling), O(N log N) ⇒ ratio ~2.1");
}
