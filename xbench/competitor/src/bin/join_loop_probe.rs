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
//! ---
//!
//! v7.39 (round 579) found what all of that was measuring. Both entries
//! cost the same in one process — `execute` 18.86 ms, the pgwire
//! read-only streaming entry 18.86 — so it was never the code path. The
//! server was measured with a FRESH `docker run psql` per query, and a
//! server asked one query per connection is a COLD server. Twelve
//! queries in ONE session:
//!
//!     43.49  22.71  19.84  19.82  18.79  18.91  18.87  18.93  18.59 …
//!
//! Steady state 18.9 ms — the embedded number exactly. There is no 2x,
//! and rounds 577 and 578 were both reading cold-start cost as if it
//! were code. PG warms up too, but less: its first query is 1.5x its
//! steady state where SPG's is 2.3x, so the fresh-client method did not
//! penalise the two engines equally.
//!
//! Warm-session medians on the same 500k table, last 6 of 10 in one
//! session:
//!
//!     single scan count       SPG  1.92 ms   PG  7.30   0.26x WIN
//!     sum agg                      2.95          7.02   0.42x WIN
//!     max+min                      4.09          7.86   0.52x WIN
//!     group by 50                  9.02         17.49   0.52x WIN
//!     join, no predicate          40.40         60.99   0.66x WIN
//!     join, both sides 100        21.49         13.60   1.58x
//!     order by desc limit 10      28.54         11.54   2.47x
//!
//! Five of seven are wins. Every number this session reported for these
//! shapes from round 567 onward was taken with one client per query and
//! is a cold reading. The losses that survive warm are ORDER BY + LIMIT
//! and the filtered join.
//!
//! One near-miss worth keeping: the first server profile this round took
//! was of a `release-dbg` binary built in round 575, BEFORE round 576's
//! bucket fix, and it still showed the allocator at 37%. That is exactly
//! the trap round 568 recorded. Rebuild the profiling binary in the
//! round you read it.

use spg_engine::{CancelToken, Engine};
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

    // v7.39 (round 579) — the two entries the two processes use, in one
    // process. `execute` takes `&mut Engine` and is what this probe has
    // always measured; `execute_readonly_select_streaming` takes `&self`
    // and is what pgwire calls. If the server's 2x lives in the code
    // path rather than the process, it shows up right here.
    let loop_for = |secs: u64, mut f: Box<dyn FnMut() + '_>| -> (u64, f64) {
        f();
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut runs = 0u64;
        let t = Instant::now();
        while Instant::now() < deadline {
            f();
            runs += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        let ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        (runs, ms)
    };

    {
        let e = &mut e;
        let (runs, ms) = loop_for(
            secs,
            Box::new(|| {
                e.execute(sql).unwrap();
            }),
        );
        println!("execute (&mut Engine)                 {runs:5} runs, {ms:6.2} ms each");
    }
    {
        let e = &e;
        let (runs, ms) = loop_for(
            secs,
            Box::new(|| {
                let mut n = 0usize;
                e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
                    if matches!(item, spg_engine::StreamItem::Row(_)) {
                        n += 1;
                    }
                    Ok(())
                })
                .unwrap();
                assert_eq!(n, 1, "one row of output");
            }),
        );
        println!("readonly streaming (&Engine, pgwire)  {runs:5} runs, {ms:6.2} ms each");
    }
}
