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
//!
//! ---
//!
//! v7.39 (round 586) put a clock inside the server's statement path,
//! temporarily, and split it:
//!
//!     first query   prologue 2003 us   executor 10434 us   total 12437
//!     second               5                    1508             1513
//!     third                2                    1735             1738
//!
//! Two costs, not one. The prologue — parse and prepared-statement
//! setup — is 2 ms once per connection and does NOT scale with rows.
//! The rest, 10.4 ms of it, is inside the executor call itself.
//!
//! And it is PER CONNECTION, not per table. In one connection: table A
//! costs 15.18 ms, then 1.76, and a table B never queried on that
//! connection costs 2.07 — no penalty at all. Reversed, the same. Round
//! 583 recorded "per connection AND per table"; the per-table half was
//! never tested, because that round's second shape used the same table.
//! It is wrong and the ledger says so.
//!
//! So: the first statement on a connection pays a first-touch cost on
//! memory that statement needs, proportional to its working set —
//! 0.13 ms for a 1000-row table, 10 ms for 500,000 — and later
//! statements on the same connection reuse it. That fits every property
//! measured: per connection, proportional to the query rather than the
//! table, nothing lasting in RSS (the arenas die with the connection),
//! not the CPU cache, and absent from this probe, whose process-wide
//! allocator was warmed by building the table.
//!
//! v7.39 (round 587) tested that and killed it, then killed its
//! successor:
//!
//!   * the arenas are NOT per connection. Every `bumpalo::Bump` in the
//!     wire path is `Bump::new()` inside the statement handler, so the
//!     second statement builds one too and would pay the same growth.
//!     Checked before writing the pool, which is the only reason no
//!     pool was written.
//!   * it is not thread-local allocator warmth either — the successor
//!     hypothesis, and a good fit: per connection is per thread, a new
//!     thread has cold magazines, and it would explain why the penalty
//!     is LARGER with sharding off (the runner threads are long-lived
//!     and already warm). A fresh thread in this probe running the same
//!     query pays nothing: 6.05, 6.54, 6.33, 6.17, 6.16, 6.13 against a
//!     steady 6.16.
//!
//! Five rounds — 583 through 587 — have not named it. What they have
//! established, all by measurement, is a long list of what it is not:
//! not the wire, not TLS, not the parallel runner, not MVCC freezing,
//! not the resident working set, not the data, not lazy decoding from
//! disk, not per query shape, not per table, not the CPU cache, not the
//! engine, not the per-statement arenas, and not thread-local allocator
//! warmth. It is per connection, it sits inside the executor call, it is
//! proportional to the first statement's working set, and it is larger
//! when sharding is off.
//!
//! That list is worth more than another round of guessing. The line
//! stops here and the ledger keeps it; picking it up again wants an
//! instrument that can attribute time inside one 10 ms event, which
//! sampling at 4 kHz cannot.

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

    // v7.39 (round 587) — the same query on a FRESH THREAD.
    //
    // The server pays its penalty once per connection, and a connection
    // is a thread. If the cost is thread-local allocator warmth — new
    // thread, cold magazines, first large allocation pays for fresh
    // spans — then a thread spawned here pays it too, with no server,
    // no socket and no session in sight. The main thread above cannot
    // show it: building the table warmed its magazines.
    std::thread::scope(|sc| {
        sc.spawn(|| {
            let e = &e;
            print!("FRESH THREAD, first six:");
            for _ in 0..6 {
                let t = Instant::now();
                let mut n = 0usize;
                e.execute_readonly_select_streaming(
                    &sql,
                    spg_engine::CancelToken::none(),
                    |item| {
                        if matches!(item, spg_engine::StreamItem::Row(_)) {
                            n += 1;
                        }
                        Ok(())
                    },
                )
                .unwrap();
                print!("  {:.2}", ms(t));
            }
            println!();
        });
    });
    print!("main thread again, three:");
    for _ in 0..3 {
        let t = Instant::now();
        e.execute(&sql).unwrap();
        print!("  {:.2}", ms(t));
    }
    println!();

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
