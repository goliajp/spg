//! v7.39 (round 576) — how many allocations does a query make?
//!
//! Round 575 profiled a self-join whose two sides are cut to 100 rows
//! out of 500k and found the ALLOCATOR at 28.1% of the connection
//! thread, about half allocation and half drop. It could not name the
//! caller: the allocator stubs are inlined into the binary, and the
//! frames a frame-pointer profile returns there carry raw addresses
//! rather than symbols, so walking past them by name matched nothing.
//!
//! A profile is the wrong instrument for that question anyway. This one
//! counts: a `#[global_allocator]` wrapper that tallies calls and bytes,
//! read before and after each query. The count against the row counts
//! the query touches says which structure it is — one allocation per
//! scanned row, per matched pair, or per query — without needing a
//! symbol at all.
//!
//! ---
//!
//! v7.39 (round 577) used it to compare the engine against the server,
//! and found the gap is not where round 576 left it. Same SQL, same
//! table shape (two INT columns, 500k rows a side):
//!
//!                        engine here   server, EXPLAIN ANALYZE   wire total   PG18
//!     join, both 100        19.0 ms          41.4-43.0             42.75      11.9
//!     join, no predicate    33.4             56.1-59.9             59.83      54.1
//!     single scan, count     6.0             10.8-15.7             16.48       7.1
//!     SELECT 1                 -                 -                  0.146      0.305
//!
//! The wire adds essentially nothing — a trivial query costs 0.146 ms
//! there, which BEATS PG's 0.305 — and `EXPLAIN ANALYZE`, which never
//! leaves the server, already carries the whole gap. Server CPU confirms
//! it: 20 joins cost 0.97 s of process CPU, 48.5 ms each, against 19.0
//! ms of single-threaded work in this probe. So the server's engine does
//! about 2.5x the CPU of the embedded engine for the same query.
//!
//! Three candidates were eliminated by measurement, not by reading:
//!
//!   * the wire — `EXPLAIN ANALYZE` (server-side, no client) reports the
//!     full time, and a trivial query is 0.146 ms;
//!   * the parallel runner — `SPG_PARALLEL=0` moves the join not at all
//!     (44.6 -> 44.4), and the join turns out not to use it, though the
//!     single scan does (14.6 on, 21.8 off);
//!   * MVCC freezing — `VACUUM FREEZE` on the table changes nothing
//!     (48.2 -> 47.7).
//!
//! What the server's engine does that `Engine::execute` does not is the
//! next round's question, and it is now a narrow one.
// A counting allocator is the one thing that cannot be written without
// `unsafe`: `GlobalAlloc` is an unsafe trait and every method forwards
// to `System`. Confined to this probe binary; nothing in the engine or
// the server relaxes the workspace rule.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks, clippy::multiple_unsafe_ops_per_block)]

use spg_engine::Engine;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every method forwards to `System`, which upholds the
// `GlobalAlloc` contract; the counters are plain relaxed atomics and do
// not touch the allocation itself.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        FREES.fetch_add(1, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new as u64, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn seed(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE j (id INT, g INT)").unwrap();
    e.execute(&format!(
        "INSERT INTO j SELECT gg, gg % 50 FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    e
}

fn measure(e: &mut Engine, sql: &str) -> (u64, u64, u64, f64) {
    // Warm once so one-off caches are not counted as the query's own.
    e.execute(sql).unwrap();
    let (a0, b0, f0) = (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
        FREES.load(Ordering::Relaxed),
    );
    let t = Instant::now();
    e.execute(sql).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    (
        ALLOCS.load(Ordering::Relaxed) - a0,
        BYTES.load(Ordering::Relaxed) - b0,
        FREES.load(Ordering::Relaxed) - f0,
        ms,
    )
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let mut e = seed(n);
    // v7.39 (round 592) — an ad-hoc query as argv[2], so a shape can be
    // counted without editing the list below.
    if let Some(sql) = std::env::args().nth(2) {
        let (allocs, bytes, frees, ms) = measure(&mut e, &sql);
        #[allow(clippy::cast_precision_loss)]
        let per_row = allocs as f64 / n as f64;
        println!(
            "{n} rows | {allocs} allocs | {frees} frees | {:.1} MB | {ms:.1} ms | {per_row:.2}/row",
            bytes as f64 / 1_048_576.0
        );
        return;
    }
    println!("{n} rows a side\n");
    println!("| query                      |    allocs |    frees |      MB |     ms | allocs/row |");
    println!("|----------------------------|----------:|---------:|--------:|-------:|-----------:|");
    for (label, sql) in [
        (
            "single scan, count",
            "SELECT count(*) FROM j WHERE id < 100",
        ),
        (
            "join, no predicate",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id",
        ),
        (
            "join, peer 100",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE b.id < 100",
        ),
        (
            "join, both 100",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 100 AND b.id < 100",
        ),
        (
            "join, both 20k",
            "SELECT count(*) FROM j a JOIN j b ON a.id = b.id WHERE a.id < 20000 AND b.id < 20000",
        ),
        // v7.39 (round 580) — the shape that survives round 579's warm
        // re-measurement as a real loss: ORDER BY + LIMIT at 2.47x.
        ("order by desc limit 1", "SELECT id FROM j ORDER BY id DESC LIMIT 1"),
        ("order by desc limit 10", "SELECT id FROM j ORDER BY id DESC LIMIT 10"),
        ("order by asc limit 10", "SELECT id FROM j ORDER BY id LIMIT 10"),
        ("order by two keys", "SELECT id FROM j ORDER BY g DESC, id DESC LIMIT 10"),
        ("max(id), same answer", "SELECT max(id) FROM j"),
    ] {
        let (allocs, bytes, frees, ms) = measure(&mut e, sql);
        #[allow(clippy::cast_precision_loss)]
        let per_row = allocs as f64 / n as f64;
        println!(
            "| {label:26} | {allocs:9} | {frees:8} | {:7.1} | {ms:6.1} | {per_row:10.2} |",
            bytes as f64 / 1_048_576.0
        );
    }
    println!("\nallocs/row is against ONE side's row count, so 2.0 means");
    println!("two allocations for every row of the table.");
}
