//! r1021 (Phase B gate) — where does a predicate's per-row cost go?
//!
//! `filtered then order` loses 1.8-2.1x to PG18 at every size, and Phase A
//! (`docs/PERF_FILTERED_THEN_ORDER_2026-08-14.md`) narrowed it to arithmetic
//! inside the predicate: `WHERE id > 0` costs ~11 ns a row, `WHERE id % 3 = 0`
//! costs ~50. The compiled VM has one recognised shape — `[Column, Lit,
//! Binary(comparison)]` — and a five-step predicate misses it.
//!
//! That much is measured. What is NOT measured is where the general loop's
//! ~10 ns a step goes, and the plan forbids attacking what has not been
//! attributed: three code-read hypotheses have already been refuted by
//! measurement in this campaign.
//!
//! So this exists to be profiled, not to print a verdict. It runs the two
//! predicates back to back over the same rows so a leaf-symbol profile can
//! be taken of each, and prints wall clock only as a sanity check that the
//! shapes still differ the way Phase A said.
//!
//!   cargo build --profile release-dbg -p spg-engine --example probe_pred_vm
//!   samply record ./target/release-dbg/examples/probe_pred_vm fast
//!   samply record ./target/release-dbg/examples/probe_pred_vm slow
//!
//! Take them SEPARATELY. One process running both gives a profile in which
//! the two loops are mixed and every symbol is a blend.

use spg_engine::Engine;
use std::time::Instant;

const ROWS: i64 = 50_000;
const REPS_DEFAULT: usize = 200;

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "both".into());
    // Long enough for a sampling profiler to collect: `sample <pid> N`
    // wants the loop still running when it attaches.
    let reps: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(REPS_DEFAULT);
    let mut e = Engine::new();
    e.set_autovacuum(false);
    e.execute("CREATE TABLE f (id BIGINT PRIMARY KEY, k BIGINT, pad TEXT)")
        .unwrap();
    for chunk in 0..(ROWS / 1000) {
        let mut sql = String::from("INSERT INTO f VALUES ");
        for i in 0..1000 {
            let id = chunk * 1000 + i + 1;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({id},{},'{}')",
                (id * 7919) % 50_000,
                "x".repeat(60)
            ));
        }
        e.execute(&sql).unwrap();
    }

    // `count(*)` so the measurement is the predicate rather than row
    // delivery — Phase A put delivery at parity with PG18 (1.03x), so it is
    // not what this is looking for.
    let run = |e: &mut Engine, sql: &str, label: &str| {
        let t0 = Instant::now();
        for _ in 0..reps {
            e.execute(sql).unwrap();
        }
        let per_row = t0.elapsed().as_secs_f64() * 1e9 / (ROWS as f64 * reps as f64);
        println!("{label:<28} {per_row:6.1} ns/row");
    };

    let fast = "SELECT count(*) FROM f WHERE id > 0";
    let slow = "SELECT count(*) FROM f WHERE id % 3 = 0";
    match which.as_str() {
        "fast" => run(&mut e, fast, "column cmp literal"),
        "slow" => run(&mut e, slow, "column arith literal cmp"),
        _ => {
            run(&mut e, fast, "column cmp literal");
            run(&mut e, slow, "column arith literal cmp");
        }
    }
}
