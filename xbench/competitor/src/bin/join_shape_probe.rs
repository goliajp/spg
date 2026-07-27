//! v7.39 (round 573) — what a self-join costs as its predicates narrow.
//!
//! Round 572 left the self-join at 3.83x with a named cause: the hash
//! build ignores a pushed predicate's selectivity, because
//! `SMALL_PEER_EAGER_ROWS = 256` decides eager pre-filtering by the
//! peer's TOTAL size. Filtering the build inside `join_stage_hash` was
//! implemented this round and measured NOTHING — `WHERE b.id < 100`,
//! which should shrink a 500k build to 99 rows, ran 72.7 ms before and
//! 71.8 after. Reverted.
//!
//! Widening the measurement said why the attack was aimed wrong. Over
//! pgwire, 500k rows both sides, one row of output, medians of 3:
//!
//!     predicate                     SPG      PG18    ratio
//!     none (500k x 500k)           84.2 ms   50.1     1.68x
//!     left side, 20k               79.2      17.2     4.61x
//!     peer side, 20k               71.2      17.7     4.03x
//!     peer side, 100               77.0      13.2     5.82x
//!     both sides, 20k              74.8      13.6     5.49x
//!     both sides, 100              72.3      12.1     5.99x
//!
//! SPG costs 71-84 ms whatever is asked. Narrowing EITHER side from
//! 500,000 rows to 100 changes nothing at all, while PG goes 50 -> 12.
//! The ratio is worst — 6x — exactly where the query asks for the least.
//!
//! So the target is not the hash build. With no predicate at all SPG is
//! 1.68x, which says the join machinery itself is competitive; what is
//! missing is any response to selectivity anywhere in the pipeline. That
//! is the next round's decomposition, and it starts by finding which
//! executor path this query actually takes — the build-side filter this
//! round added was correct code that never ran, and the round did not
//! establish where it should have gone instead.

use spg_engine::Engine;
use std::time::Instant;

fn seed(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE j (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO j SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    e
}

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
        .unwrap_or(200_000);
    let mut e = seed(n);
    println!("{n} rows each side, engine-side, median of 5\n");
    println!("| predicate            |     ms |");
    println!("|----------------------|-------:|");
    for (label, sql) in [
        ("none", "SELECT count(*) FROM j a JOIN j b ON a.id = b.id"),
        (
            "left 20k",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 20000",
        ),
        (
            "peer 20k",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE b.id < 20000",
        ),
        (
            "peer 100",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE b.id < 100",
        ),
        (
            "both 20k",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 20000 AND b.id < 20000",
        ),
        (
            "both 100",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 100 AND b.id < 100",
        ),
    ] {
        println!("| {label:20} | {:6.2} |", median(&mut e, sql, 5));
    }
    println!("\nA row that costs the same whatever is asked of it is not");
    println!("bounded by the work the query needs.");
}
