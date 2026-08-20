//! v7.38.8 Phase A — what does a per-row predicate actually spend?
//!
//! The ablation in docs/PERF-FINDING-2026-08-20-sentori-shapes.md put
//! prices on three predicates over the customer's shape: an integer
//! comparison at 22 ns a row, a timestamp comparison at 52, and a jsonb
//! field access at 398. It could not say what any of them spends the
//! time ON — and the one mechanism proposed from reading the code was
//! refuted by its own discriminator.
//!
//! So this exists to be profiled, not to be timed: one table, one query
//! per shape, repeated enough that the leaf symbols separate.
//!
//!   cargo build --profile release-dbg --example probe_predicate_cost
//!   samply record ./target/release-dbg/examples/probe_predicate_cost ts
//!
//! The argument picks the shape so a profile carries one predicate and
//! not a mixture: `bare`, `int`, `ts`, `json`.

use spg_engine::Engine;

fn main() {
    let shape = std::env::args().nth(1).unwrap_or_else(|| "ts".into());
    let rows: i64 = std::env::var("ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE events (id BIGINT NOT NULL, project_id INT NOT NULL, \
         kind TEXT NOT NULL, traits JSONB NOT NULL, received_at TIMESTAMP NOT NULL)",
    )
    .unwrap();
    for i in 1..=rows {
        eng.execute(&format!(
            "INSERT INTO events VALUES ({i}, {}, '{}', '{{\"plan\":\"{}\",\"country\":\"jp\",\"version\":\"{}\",\"seat\":{}}}', \
             timestamp '2026-05-01 00:00:00' + interval '{} minutes')",
            (i % 8) + 1,
            ["open", "click", "deliver", "bounce"][(i % 4) as usize],
            ["free", "pro", "team"][(i % 3) as usize],
            (i % 40) + 1,
            i % 500,
            i % 129_600,
        ))
        .unwrap();
    }

    let sql = match shape.as_str() {
        "bare" => "SELECT count(*) FROM events",
        "int" => "SELECT count(*) FROM events WHERE id > 0",
        "ts" => "SELECT count(*) FROM events WHERE received_at > timestamp '1900-01-01'",
        "json" => "SELECT count(*) FROM events WHERE traits->>'plan' = 'pro'",
        other => panic!("unknown shape {other} — bare | int | ts | json"),
    };

    // Warm once outside the measured window so a first-call cost does
    // not land in the profile as if it were per-row work.
    eng.execute(sql).unwrap();

    // Does the ColumnCmpLit fast lane decide this predicate, or does it
    // decline once per row and leave the general machine to answer? The
    // counter is the only witness that separates those two — the prices
    // look the same from outside.
    #[cfg(feature = "perf-counters")]
    {
        use core::sync::atomic::Ordering::Relaxed;
        let f0 = spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed);
        let i0 = spg_engine::eval::compiled::STEP_VM_INTLANE_FIRE.load(Relaxed);
        let b0 = spg_engine::eval::compiled::STEP_VM_INTLANE_FALLBACK.load(Relaxed);
        eng.execute(sql).unwrap();
        println!(
            "{shape}: over {rows} rows — fastpred fired {}, intlane fired {}, intlane fell back {}",
            spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed) - f0,
            spg_engine::eval::compiled::STEP_VM_INTLANE_FIRE.load(Relaxed) - i0,
            spg_engine::eval::compiled::STEP_VM_INTLANE_FALLBACK.load(Relaxed) - b0,
        );
    }
    let t0 = std::time::Instant::now();
    for _ in 0..reps {
        eng.execute(sql).unwrap();
    }
    let el = t0.elapsed();
    println!(
        "{shape}: {rows} rows x {reps} reps in {:?} — {:.3} ms/query, {:.1} ns/row",
        el,
        el.as_secs_f64() * 1000.0 / reps as f64,
        el.as_secs_f64() * 1e9 / (reps as f64 * rows as f64),
    );
}
