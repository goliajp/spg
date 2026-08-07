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

/// Single multi-row INSERT of N non-overlapping ranges — exercises the
/// intra-batch pairwise check (O(N^2) today) with an empty existing table.
fn bench_batch(n: usize) -> f64 {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bk (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    let mut sql = String::from("INSERT INTO bk VALUES ");
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("('[{},{})')", 2 * i, 2 * i + 1));
    }
    let start = Instant::now();
    e.execute(&sql).unwrap();
    start.elapsed().as_secs_f64()
}

/// Build N non-overlapping rows, then UPDATE each to a fresh non-overlapping
/// range (single-row UPDATEs) — exercises the existing-row scan on the UPDATE
/// path. O(n)/update → O(N^2); the index makes each O(log n) → O(N log N).
fn bench_update(n: usize) -> f64 {
    let mut e = Engine::new();
    // id PRIMARY KEY so the `WHERE id = i` lookup is O(log n) and doesn't
    // itself contribute an O(n) scan that would mask the EXCLUDE cost.
    e.execute(
        "CREATE TABLE bk (id int PRIMARY KEY, during int4range, \
         EXCLUDE USING gist (during WITH &&))",
    )
    .unwrap();
    for i in 0..n {
        e.execute(&format!(
            "INSERT INTO bk VALUES ({i}, '[{},{})')",
            2 * i,
            2 * i + 1
        ))
        .unwrap();
    }
    let base = 2 * n + 10;
    let start = Instant::now();
    for i in 0..n {
        let lo = base + 2 * i;
        e.execute(&format!(
            "UPDATE bk SET during = '[{lo},{})' WHERE id = {i}",
            lo + 1
        ))
        .unwrap();
    }
    start.elapsed().as_secs_f64()
}

fn main() {
    println!(
        "{:>8}  {:>10}  {:>12}  {:>8}",
        "N", "total_s", "us/insert", "ratio"
    );
    let mut prev: Option<(usize, f64)> = None;
    for &n in &[1000usize, 2000, 4000, 8000] {
        let t = bench(n);
        let us = t / n as f64 * 1e6;
        let ratio = prev.map(|(_, pt)| t / pt).unwrap_or(0.0);
        println!("{n:>8}  {t:>10.4}  {us:>12.2}  {ratio:>8.2}");
        prev = Some((n, t));
    }
    println!("\nO(N^2) ⇒ ratio ~4.0 (each N-doubling), O(N log N) ⇒ ratio ~2.1");

    println!("\n-- single multi-row INSERT (intra-batch pairwise) --");
    println!("{:>8}  {:>10}  {:>8}", "N", "total_s", "ratio");
    let mut prev: Option<f64> = None;
    for &n in &[1000usize, 2000, 4000, 8000] {
        let t = bench_batch(n);
        let ratio = prev.map(|pt| t / pt).unwrap_or(0.0);
        println!("{n:>8}  {t:>10.4}  {ratio:>8.2}");
        prev = Some(t);
    }

    println!("\n-- single-row UPDATE stream (existing-row scan) --");
    println!("{:>8}  {:>10}  {:>8}", "N", "total_s", "ratio");
    let mut prev: Option<f64> = None;
    for &n in &[1000usize, 2000, 4000, 8000] {
        let t = bench_update(n);
        let ratio = prev.map(|pt| t / pt).unwrap_or(0.0);
        println!("{n:>8}  {t:>10.4}  {ratio:>8.2}");
        prev = Some(t);
    }
}
