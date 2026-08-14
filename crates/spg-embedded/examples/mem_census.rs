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
            let total = TOTAL.fetch_add(l.size(), Relaxed) + l.size();
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
            maybe_sample(total);
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

// r1027 — WHERE the bytes come from, not just how many.
//
// Two hypotheses about this import's 15.2 GB of allocation have now been
// refuted by measurement — that per-trigram `String`s dominated it (removing
// them took 9 %), and that copy-on-write node copies dragged whole posting
// lists (putting the lists behind an `Arc` moved nothing). Both were
// arithmetic fitted to a plausible mechanism, and the arithmetic fit both
// times.
//
// A total tells you how much. It cannot tell you who, and guessing who is
// what has cost two rounds. So: sample a backtrace every `SAMPLE_EVERY`
// bytes and aggregate the frames. The allocator itself allocates when it
// captures one, hence the re-entry guard — without it the first sample
// recurses until the stack ends.
const SAMPLE_EVERY: usize = 4 * 1024 * 1024;
static NEXT_SAMPLE: AtomicUsize = AtomicUsize::new(SAMPLE_EVERY);

std::thread_local! {
    static IN_SAMPLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static SAMPLES: std::sync::Mutex<Vec<std::string::String>> = std::sync::Mutex::new(Vec::new());

fn maybe_sample(total_after: usize) {
    if total_after < NEXT_SAMPLE.load(Relaxed) {
        return;
    }
    NEXT_SAMPLE.store(total_after + SAMPLE_EVERY, Relaxed);
    let already = IN_SAMPLER.with(|f| f.replace(true));
    if already {
        return;
    }
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    if let Ok(mut v) = SAMPLES.lock() {
        v.push(bt);
    }
    IN_SAMPLER.with(|f| f.set(false));
}

/// The deepest frame naming spg code — the allocation's owner, as opposed to
/// the `Vec::reserve` or `RawVec::grow` that literally asked.
fn owner_frame(bt: &str) -> std::string::String {
    for line in bt.lines() {
        let l = line.trim();
        if let Some(rest) = l.split_once("at ").map(|(_, r)| r)
            && (rest.contains("/spg-") || rest.contains("crates/spg"))
            && !rest.contains("mem_census")
        {
            return rest.split('/').next_back().unwrap_or(rest).to_string();
        }
    }
    for line in bt.lines() {
        let l = line.trim();
        if l.contains("spg_storage::") || l.contains("spg_engine::") {
            let cut = l.split_once(": ").map_or(l, |(_, r)| r);
            return cut.chars().take(90).collect();
        }
    }
    "<no spg frame>".to_string()
}

fn report_samples() {
    let Ok(v) = SAMPLES.lock() else { return };
    let mut counts: std::collections::BTreeMap<std::string::String, usize> =
        std::collections::BTreeMap::new();
    for bt in v.iter() {
        *counts.entry(owner_frame(bt)).or_insert(0) += 1;
    }
    let mut rows: Vec<(std::string::String, usize)> = counts.into_iter().collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.1));
    println!(
        "\n=== allocation samples ({} at one per {} MiB = {:.1} GB attributed)",
        v.len(),
        SAMPLE_EVERY / 1_048_576,
        v.len() as f64 * SAMPLE_EVERY as f64 / 1e9
    );
    for (frame, n) in rows.iter().take(18) {
        println!(
            "  {:5}  {:5.1} GB  {frame}",
            n,
            *n as f64 * SAMPLE_EVERY as f64 / 1e9
        );
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
    report_samples();
}
