//! v7.39 (round 561) — split what round 560 left, and correct what it
//! recorded.
//!
//! Round 560 attributed its remainder to "one Row allocation per output
//! row". That was an inference, not a measurement, so this splits it:
//! the index walk that builds (key, locator) pairs, against the row
//! construction that follows. On 500k rows:
//!
//!     index-only  SELECT k      3.08 ms      = 31 ns/row
//!     row-fetch   SELECT id    10.05 ms
//!     no output   count(*)      0.99 ms
//!
//! So the engine does the whole 100k-row scan in 3.08 ms, and the
//! inference was wrong — nothing near 18 ms is in the executor.
//!
//! Re-measuring over the wire then found the recorded GAP wrong too.
//! Round 560 had PG on container loopback and SPG through Docker's host
//! NAT; on a 1.2 MB result that difference is most of what was being
//! compared. Same client for both, interleaved, medians: PG 13.2 ms,
//! SPG 24.4 ms — 1.85×, not the 8× on record. At one row SPG wins,
//! 0.28 against 0.58.
//!
//! That leaves ~21 ms wire-side against PG's ~13, and one hypothesis
//! with a name: the simple-query path encodes every row into `wbuf` and
//! releases it in a single `write_all` at the end — deliberately, per
//! its own comment, "so the final write_all is still a single TCP
//! syscall". PG flushes as its 8 kB buffer fills, so its encoding
//! overlaps the client's parsing where SPG's cannot.
//!
//! Implemented (flush at 64 kB) and measured A/B on one data directory,
//! 9 runs each, alternating:
//!
//!     single write_all   median 22.7 ms   mean 23.1   range 14.6-28.0
//!     64 kB chunks       median 24.9 ms   mean 25.1   range 22.6-28.0
//!
//! No gain, slightly worse, spreads overlapping — REFUTED and reverted.
//! Not bisected for a better threshold: the hypothesis was that the
//! overlap was worth having, and the measurement says it is not. Where
//! the ~21 ms actually goes is still unnamed.
use spg_engine::Engine;
use std::time::Instant;

fn seed(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE i (id INT, k INT, pad TEXT)").unwrap();
    e.execute(&format!(
        "INSERT INTO i SELECT g, g, repeat('x', 100) FROM generate_series(1, {n}) g"
    ))
    .unwrap();
    e.execute("CREATE INDEX ik ON i (k)").unwrap();
    e
}

fn best(e: &mut Engine, sql: &str) -> f64 {
    let mut b = f64::MAX;
    for _ in 0..9 {
        let t = Instant::now();
        e.execute(sql).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms < b {
            b = ms;
        }
    }
    b
}

fn main() {
    let mut e = seed(500_000);
    println!("| rows out | index-only ms | us/row |");
    for (lo, hi) in [(1i64, 1_000i64), (1, 10_000), (1, 100_000)] {
        let n = hi - lo + 1;
        let ms = best(
            &mut e,
            &format!("SELECT k FROM i WHERE k BETWEEN {lo} AND {hi}"),
        );
        println!("| {n:>8} | {ms:>13.2} | {:>6.2} |", ms * 1000.0 / n as f64);
    }
    // The same range with the row actually needed — how much of the
    // cost is NOT the row fetch.
    println!();
    println!("| shape                                  | 100k-row range ms |");
    for (label, sql) in [
        ("index-only  SELECT k    (this path)  ", "SELECT k FROM i WHERE k BETWEEN 1 AND 100000"),
        ("row-fetch   SELECT id   (old path)   ", "SELECT id FROM i WHERE k BETWEEN 1 AND 100000"),
        ("row-fetch   SELECT k,id (old path)   ", "SELECT k, id FROM i WHERE k BETWEEN 1 AND 100000"),
        ("no output   count(*)    (aggregate)  ", "SELECT count(*) FROM i WHERE k BETWEEN 1 AND 100000"),
    ] {
        println!("| {label} | {:>17.2} |", best(&mut e, sql));
    }
}
