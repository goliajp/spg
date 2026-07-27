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
