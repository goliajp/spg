//! v7.39 (round 578) — one join, in a loop, so it can be profiled
//! against the server running the same query.
//!
//! Round 577 established that the server's engine costs about 2.5x the
//! embedded one for the same SQL on the same data — 48.5 ms of process
//! CPU per join against 19.0 ms here — and eliminated the wire, the
//! parallel runner and MVCC freezing. What remains is a question about
//! two code paths, and the way to answer it is to profile both and
//! subtract.
//!
//! This binary exists to be the second profile. It does nothing but seed
//! the table and run the one query until told to stop, so its profile is
//! that query and nothing else.
//!
//! What round 578 got out of it:
//!
//!   * the embedded number is solid — steady state 19.09 and 21.13 ms
//!     across two 12-second runs, agreeing with `alloc_count_probe`'s
//!     single-shot 19.0, against the server's 41-43;
//!   * the two profiles have the SAME structure and nearly the same
//!     distribution — `build_joined_filtered_rows` ~30%,
//!     `filter_table_indices` ~22%, `eval_compiled_pred` ~10% on both
//!     sides. The server is not doing extra work anywhere; every part of
//!     the same work costs about twice as much;
//!   * it is not the resident working set. A server started on an empty
//!     data directory with ONLY this table runs the query in 39.96 ms,
//!     and adding three more 500k-row tables leaves it at 38.05;
//!   * it is not the data. The server table has exactly 500,000 rows,
//!     500,000 distinct ids and no dead tuples.
//!
//! With rounds 577's eliminations (the wire, the parallel runner, MVCC
//! freezing) that leaves something that makes ALL work uniformly slower
//! in the server process rather than any one stage — which is where the
//! next round starts.
//!
//! One near-miss worth keeping: the first server profile this round took
//! was of a `release-dbg` binary built in round 575, BEFORE round 576's
//! bucket fix, and it still showed the allocator at 37%. That is exactly
//! the trap round 568 recorded. Rebuild the profiling binary in the
//! round you read it.

use spg_engine::Engine;
use std::time::{Duration, Instant};

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000);
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let mut e = Engine::new();
    e.execute("CREATE TABLE j (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO j SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    let sql = "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 100 AND b.id < 100";
    e.execute(sql).unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut runs = 0u64;
    let t = Instant::now();
    while Instant::now() < deadline {
        e.execute(sql).unwrap();
        runs += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
    println!("{runs} runs, {ms:.2} ms each");
}
