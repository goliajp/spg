//! r1019 (Phase A3) — how much does an import actually ALLOCATE, against
//! how much the process ends up resident?
//!
//! After `--batch-commit` (B1), loading mailrs's 95 MB seed still peaks at
//! 1,010 MB on a primary-key-only schema whose finished catalog is 213 MB
//! and whose script is 95 MB. Roughly 700 MB is unaccounted, and the plan
//! forbids attacking what has not been named.
//!
//! Peak RSS cannot tell the two candidates apart. A process that allocates
//! 900 MB of live structures and one that allocates 250 MB while churning
//! 650 MB of short-lived garbage the allocator never returns to the OS look
//! identical from outside. They need opposite fixes: the first wants smaller
//! structures, the second wants less churn or an allocator that purges.
//!
//! So this counts allocation directly. A wrapper around the system allocator
//! tracks live bytes, its high-water mark, and the total ever allocated. The
//! gap between peak live and peak RSS is the allocator's retention, and it is
//! a number rather than an argument.
//!
//! Peak RSS comes from `/usr/bin/time -l` around the run rather than from
//! inside it — one number from the OS, no dependency, and it is the same
//! figure every other measurement in this campaign used.
//!
//!   /usr/bin/time -l cargo run --release --example mem_census \
//!       -- <db.spg> <script.sql> [batch]

// The counting allocator is the point of this example.
#![allow(unsafe_code)]

use spg_embedded::Database;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// Relaxed ordering throughout: these are diagnostics, and a racy peak that
// misses a few bytes on a concurrent thread does not change any conclusion
// drawn at this scale. The import itself is single-threaded.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            TOTAL.fetch_add(l.size(), Relaxed);
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) };
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                TOTAL.fetch_add(new - l.size(), Relaxed);
                let now = LIVE.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn report(stage: &str) {
    println!(
        "{stage:<26} live {:8.1} MB   peak-live {:8.1} MB   ever {:9.1} MB",
        mb(LIVE.load(Relaxed)),
        mb(PEAK.load(Relaxed)),
        mb(TOTAL.load(Relaxed)),
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .expect("usage: mem_census <db.spg> <script.sql> [batch]");
    let script_path = args
        .next()
        .expect("usage: mem_census <db.spg> <script.sql> [batch]");
    let batch: Option<usize> = args.next().and_then(|s| s.parse().ok());

    report("start");
    let script = std::fs::read_to_string(&script_path).expect("read script");
    report("script read");

    let mut db = Database::open_path(&db_path).expect("open db");
    report("catalog open");

    let statements = spg_embedded::split_statements(&script);
    let wrap = statements.len() > 1;
    if wrap {
        db.execute("BEGIN").expect("BEGIN");
    }
    for (i, stmt) in statements.iter().enumerate() {
        db.execute_dump_statement(stmt)
            .unwrap_or_else(|e| panic!("statement #{}: {e:?}", i + 1));
        if wrap
            && let Some(n) = batch
            && (i + 1) % n == 0
            && i + 1 < statements.len()
        {
            db.execute("COMMIT").expect("COMMIT");
            db.execute("BEGIN").expect("BEGIN");
        }
        if (i + 1) % 20 == 0 {
            report(&format!("after stmt {}", i + 1));
        }
    }
    if wrap {
        db.execute("COMMIT").expect("COMMIT");
    }
    report("committed");

    // Drop the script and the statement slices; whatever stays live after
    // this is the engine's, not the importer's.
    drop(statements);
    drop(script);
    report("script dropped");

    let snap = db.snapshot();
    println!("snapshot bytes            {:8.1} MB", mb(snap.len()));
    report("snapshot built");
    drop(snap);
    report("snapshot dropped");

    drop(db);
    report("db dropped");
}
