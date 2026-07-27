//! v7.39 (round 568) — which aggregate costs what, over one scan.
//!
//! Round 567 swept the read panel and found the loss to PG18 is broad
//! and lives in the sequential scan: 2-4x on every shape that touches
//! rows, at 500k. This asks a narrower question the sweep raised —
//! several of those shapes read exactly the same rows, so what differs?
//!
//! Over pgwire, 500k INT rows, medians of 3 (each returns ONE row, so
//! the client and the transport are out of the picture):
//!
//!     sum(id)         13.4 ms      PG 8.2      1.63x
//!     count(id)       15.2         PG 8.0      1.90x
//!     avg(id)         15.2         PG 8.6      1.76x
//!     sum(id),sum(g)  15.0         PG 9.5      1.57x
//!     max(id)         27.6         PG 8.2      3.39x
//!     min(id)         26.5         PG 8.4      3.17x
//!     max(id),min(id) 32.1         PG 9.2      3.48x
//!
//! A SECOND aggregate is nearly free — two sums cost what one costs. But
//! `max` and `min` cost DOUBLE a sum over the same scan, and PG is flat
//! at 8.2 across all of them. So the extra is not the scan and not the
//! number of aggregates: it is what min/max do per row.
//!
//! `min(id)` is the clean half of that. Over ascending ids the minimum
//! settles on the first row and never updates again, so it clones
//! nothing — what it pays is the comparison alone, and it still costs
//! what `max` costs. 13 ms over 500k rows is 26 ns for comparing two
//! i32s.
//!
//! An attack on that was refuted. `value_cmp` reaches its integer arms
//! fifth, after a call into `numeric_bignum_cmp` and two matches
//! building a `NumericKind` per side; shortcutting the three
//! same-width integer pairs at the top measured nothing at all (max
//! 28.7 -> 30.0, min 24.8 -> 24.6, both 32.5 -> 32.9). Reverted. The
//! compiler was evidently already collapsing that prelude.
//!
//! What the profile says instead, on a binary built from the current
//! tree — `SELECT min(id)`, connection thread, self time:
//!
//!     accumulate_groups  (index.rs)   13.19%   <- the largest symbol
//!     value_cmp                        8.02%
//!     extreme_cmp                      3.87%
//!     accumulate_groups  (macros.rs)   3.64%
//!     the scan lines                  ~9%
//!
//! The largest single cost is INDEXING inside `accumulate_groups` — a
//! query with no GROUP BY still walks the grouped machinery per row,
//! indexing `row_eval_cache[s]`, `arg2_literal_val[i]`, `entry.1[i]`
//! and the rest for every row of a single group. That is a refactor,
//! not a hoist, and it is the next round's subject.
//!
//! Engine-side, this probe on 500k rows (median of 7) says the same
//! thing without a wire in the way:
//!
//!     sum 4.02 ms   count 3.31   avg 3.62   sum,sum 4.48
//!     max 7.74      min 6.45     max,min 10.97
//!     max(g) 6.99   <- settles on 49 early, so it clones as rarely as
//!                      min does, and costs what min costs
//!
//! So the clone is not where the difference lives.
//!
//! One methodological note worth keeping: the first profile this round
//! took was of a `release-dbg` binary built BEFORE round 567's cursor
//! landed, and it still showed the scan line at 20.75%. A profile is
//! only about the code it was built from — rebuild it in the same round
//! you read it.

use spg_engine::Engine;
use std::time::Instant;

fn seed(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO a SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    e
}

/// Median of `runs`, so one scheduling hiccup does not set the number.
fn median(e: &mut Engine, sql: &str, runs: usize) -> f64 {
    let mut v: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            e.execute(sql).unwrap();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    v[v.len() / 2]
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000);
    let mut e = seed(n);
    println!("{n} rows, engine-side, median of 7\n");
    println!("| aggregate            |     ms | ns/row |");
    println!("|----------------------|-------:|-------:|");
    for sql in [
        "SELECT sum(id) FROM a",
        "SELECT count(id) FROM a",
        "SELECT avg(id) FROM a",
        "SELECT sum(id), sum(g) FROM a",
        "SELECT max(id) FROM a",
        "SELECT min(id) FROM a",
        "SELECT max(id), min(id) FROM a",
        // A max whose extreme settles early, so it clones as rarely as
        // min does — if this costs what max(id) costs, the clone is not
        // where the difference lives.
        "SELECT max(g) FROM a",
    ] {
        let ms = median(&mut e, sql, 7);
        let label = sql.trim_start_matches("SELECT ").trim_end_matches(" FROM a");
        println!(
            "| {label:20} | {ms:6.2} | {:6.1} |",
            ms * 1_000_000.0 / n as f64
        );
    }
}
