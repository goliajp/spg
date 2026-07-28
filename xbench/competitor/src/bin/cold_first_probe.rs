//! v7.39 (round 585) — is the cold penalty reproducible without a
//! server?
//!
//! Rounds 583 and 584 characterised a first-query cost that is
//! proportional to the table (about 20 ns a row), is not the CPU cache,
//! is not lazy decoding from disk, is not TLS, and looked per
//! connection. Two rounds of measuring around the connection produced
//! no mechanism, and round 584 showed why: a whole-thread profile
//! includes the handshake, and psql's timing does not.
//!
//! So: drop the server. If a fresh `Engine` shows the same first-query
//! cost, the effect is per-Engine-and-table rather than per connection,
//! and it can be bisected in-process with nothing but a clock.
//!
//! Prints the first N executions individually, then the steady state.
//!
//! The answer is NO — and that is the useful part. On 500k rows:
//!
//!     first eight   7.59  6.03  6.15  6.13  6.14  6.14  6.14  6.13
//!     steady n=40   min 5.99  median 6.02  max 6.72
//!     a SECOND 500k table, first four   6.09  6.36  6.47  6.48
//!
//! The first query is 1.5 ms above steady, and a brand-new second table
//! costs nothing extra at all. Against the server, where the first query
//! on a table in a connection runs 12-31 ms against a steady 2-6.5 and
//! every new connection pays it again, that places the whole effect in
//! the SERVER layer rather than the engine.
//!
//! The steady number here, 6.0 ms, matches the server with
//! `SPG_PARALLEL=0` (6.5) rather than its parallel median (2.0), which
//! is expected: this probe installs no parallel runner. Note that the
//! server's penalty is LARGEST with sharding OFF — 31.4 against a steady
//! 6.5 — so it is not the shard threads spinning up.

use spg_engine::Engine;
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000);
    let sql: String = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "SELECT count(*) FROM c WHERE id < 100".into());

    let t = Instant::now();
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO c SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    println!("{n} rows, built in {:.1} ms\n", ms(t));

    print!("first eight executions:");
    for _ in 0..8 {
        let t = Instant::now();
        e.execute(&sql).unwrap();
        print!("  {:.2}", ms(t));
    }
    println!();

    let mut v: Vec<f64> = (0..40)
        .map(|_| {
            let t = Instant::now();
            e.execute(&sql).unwrap();
            ms(t)
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    println!(
        "steady (n=40): min {:.2}  median {:.2}  max {:.2}",
        v[0],
        v[v.len() / 2],
        v[v.len() - 1]
    );

    // A SECOND table of the same size, first touched now — if the cost
    // is per table rather than per process, this pays it again.
    e.execute("CREATE TABLE c2 (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO c2 SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    let sql2 = sql.replace("FROM c ", "FROM c2 ").replace("FROM c\n", "FROM c2\n");
    print!("second table, first four:");
    for _ in 0..4 {
        let t = Instant::now();
        e.execute(&sql2).unwrap();
        print!("  {:.2}", ms(t));
    }
    println!();
}
