//! r1030 — `SELECT DISTINCT k ORDER BY k` where every k is already unique.
//!
//! The 400 k sweep cell `distinct then order` is the one shape at that size
//! where PG18's floor sits below ours (99.7 ms against 123.2 ms at N=21).
//! Two cells of the SAME run price the step, so the route constant cancels:
//!
//!   SELECT k FROM t ORDER BY k            75.0-81.3 ms
//!   SELECT DISTINCT k FROM t ORDER BY k  123.2-138.9 ms
//!
//! About 50 ms for a DISTINCT that removes nothing — the sweep's `k` is
//! `(g * 7919) % rows`, and 7919 is coprime with the row count, so the
//! column is a permutation and every row survives.
//!
//! That is the same ablation this probe runs in-process, and the target a
//! profile should be pointed at. `distinct` and `plain` differ only in the
//! keyword, so their difference is the dedup step and nothing else.
//!
//! Usage — one query per run, so a profile of one is not diluted by the
//! other:
//!   cargo run --profile release-dbg --example probe_distinct_unique -- distinct [reps]
//!   cargo run --profile release-dbg --example probe_distinct_unique -- plain    [reps]
//!   cargo run --profile release-dbg --example probe_distinct_unique -- both

// A counting global allocator is the instrument; the workspace denies
// unsafe, and `GlobalAlloc` cannot be implemented without it.
#![allow(unsafe_code)]

use spg_engine::{Engine, TempRun, TempStoreError};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Both legs profile as allocator-dominated, which says nothing about which
/// one allocates MORE — the profile is per wall-clock and the legs run at
/// different speeds. A count per query answers it directly.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// Sampling attribution. The counts above say HOW MANY; they cannot say
/// WHICH SITE, and this line has already been wrong twice about a mechanism
/// it reasoned out instead of measuring. Sampling by allocation COUNT and
/// not by bytes is the point: the allocations in question are tiny, and
/// byte-sampling would barely see them.
///
/// Capturing a backtrace allocates, hence the re-entry guard.
const SAMPLE_EVERY: u64 = 16_384;
static NEXT_SAMPLE: AtomicU64 = AtomicU64::new(SAMPLE_EVERY);
static SAMPLES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static DEPTH: AtomicU64 = AtomicU64::new(0);

std::thread_local! {
    static IN_SAMPLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Attribution is opt-in (`SPG_PROBE_SAMPLE=1`). Capturing thousands of
/// backtraces inside the timed window would be paid by the clock, and this
/// probe's whole job is to compare two clocks.
static SAMPLING: AtomicU64 = AtomicU64::new(0);

/// Resolve the switch ONCE, from `main`, before anything is measured.
///
/// It used to be resolved lazily from inside `maybe_sample` — and
/// `std::env::var` allocates. That allocation re-entered `maybe_sample`
/// before the threshold had been advanced and before the re-entry guard
/// had been taken, so it called `env::var` again, and again: an infinite
/// recursion that presented as a stack overflow. The re-entry counter
/// never fired because the recursion happened upstream of it.
fn init_sampling() {
    let on = u64::from(std::env::var("SPG_PROBE_SAMPLE").as_deref() == Ok("1"));
    SAMPLING.store(on, Relaxed);
}

fn sampling_on() -> bool {
    SAMPLING.load(Relaxed) == 1
}

/// Resolving a backtrace to function names is the expensive half, and on a
/// `release-dbg` binary this size it is SECONDS per sample. Keep few enough
/// that the report cannot turn into a hang: the question is which sites
/// dominate, and a hundred samples answers that as well as a thousand.
const MAX_SAMPLES: usize = 512;

fn maybe_sample(count_after: u64) {
    if count_after < NEXT_SAMPLE.load(Relaxed) {
        return;
    }
    if !sampling_on() {
        NEXT_SAMPLE.store(count_after + SAMPLE_EVERY, Relaxed);
        return;
    }
    NEXT_SAMPLE.store(count_after + SAMPLE_EVERY, Relaxed);
    if IN_SAMPLER.with(|f| f.replace(true)) {
        return;
    }
    // A re-entry check that does NOT allocate. The earlier version
    // reported through `eprintln!`, which allocates, so its silence
    // proved nothing — it could have overflowed before printing.
    // `abort` here is distinguishable from a stack overflow by the
    // signal alone, which is the whole point.
    if DEPTH.fetch_add(1, Relaxed) != 0 {
        std::process::abort();
    }
    // Resolve HERE, at capture. Deferring resolution to the report was
    // tried, on the theory that symbolising inside the allocator was what
    // overflowed the stack; it was not — that was the `env::var` recursion
    // fixed above — and deferring made the report take tens of minutes
    // where resolving at capture takes seconds. The guard makes this safe.
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    DEPTH.fetch_sub(1, Relaxed);
    // `try_lock`, never `lock`: this runs INSIDE the allocator, and
    // blocking here on a lock some other allocating code holds is a
    // deadlock. Dropping a sample is the correct loss.
    if let Ok(mut v) = SAMPLES.try_lock()
        && v.len() < MAX_SAMPLES
    {
        v.push(bt);
    }
    IN_SAMPLER.with(|f| f.set(false));
}

/// The deepest frame naming engine code — the site that wanted the memory,
/// rather than the `RawVec::grow` that literally asked for it.
fn owner_frame(bt: &str) -> String {
    for line in bt.lines() {
        let l = line.trim();
        if (l.contains("spg_engine::") || l.contains("spg_storage::"))
            && !l.contains("probe_distinct_unique")
        {
            let cut = l.split_once(": ").map_or(l, |(_, r)| r);
            return cut.chars().take(100).collect();
        }
    }
    // Say which frame WAS seen rather than only that none matched: a
    // build without debug info resolves to something, just not to a
    // name this filter recognises, and reporting `<none>` for all of
    // them hides that the run worked and the filter did not.
    for line in bt.lines() {
        let l = line.trim();
        if !l.is_empty() && !l.starts_with("stack backtrace") {
            return alloc_fmt_unmatched(l);
        }
    }
    "<empty backtrace>".to_string()
}

fn alloc_fmt_unmatched(line: &str) -> String {
    let cut: String = line.chars().take(80).collect();
    format!("[no engine frame] {cut}")
}

fn report_samples(label: &str) {
    // Take the samples OUT and drop the lock before aggregating. Holding
    // it while building the map deadlocks: the map allocates, the
    // allocator samples, and the sampler wants this same lock. Cost five
    // hours of a hung run to find.
    let taken: Vec<String> = match SAMPLES.lock() {
        Ok(mut v) => core::mem::take(&mut *v),
        Err(_) => return,
    };
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for bt in taken.iter() {
        *counts.entry(owner_frame(bt)).or_insert(0) += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.1));
    println!("\n{label}: allocation owners, one sample per {SAMPLE_EVERY} allocations");
    for (frame, n) in rows.iter().take(12) {
        println!(
            "  {:>6}k allocs   {frame}",
            n * SAMPLE_EVERY as usize / 1000
        );
    }
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let n = ALLOCS.fetch_add(1, Relaxed) + 1;
        ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        let p = unsafe { System.alloc(layout) };
        maybe_sample(n);
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A spill sink, so the probe can be run in BOTH configurations. The
/// server installs one and `Engine::new()` does not, and that difference
/// decides which executor serves an ORDER BY: with a sink present the
/// spill sorter takes the query before any later lane sees it. A probe
/// that only ever ran without one measured a lane the server never
/// reaches (r1031).
struct MemRun {
    buf: Vec<u8>,
    read_at: usize,
}

impl TempRun for MemRun {
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError> {
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
    fn seal(&mut self) -> Result<(), TempStoreError> {
        self.read_at = 0;
        Ok(())
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TempStoreError> {
        let n = core::cmp::min(buf.len(), self.buf.len() - self.read_at);
        buf[..n].copy_from_slice(&self.buf[self.read_at..self.read_at + n]);
        self.read_at += n;
        Ok(n)
    }
    fn bytes_written(&self) -> u64 {
        self.buf.len() as u64
    }
}

fn mem_run() -> Result<Box<dyn TempRun>, TempStoreError> {
    Ok(Box::new(MemRun {
        buf: Vec::new(),
        read_at: 0,
    }))
}

const ROWS: i64 = 400_000;
/// Coprime with `ROWS`, so `k` is a permutation of `0..ROWS` and DISTINCT
/// drops nothing. Same generator as `scripts/perf-endpoint-sweep.sh`.
const STRIDE: i64 = 7919;

fn seed(eng: &mut Engine) {
    eng.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT NOT NULL)")
        .expect("create");
    let mut sql = String::with_capacity(1 << 20);
    let mut i = 1;
    while i <= ROWS {
        sql.clear();
        sql.push_str("INSERT INTO t VALUES ");
        let end = (i + 4_999).min(ROWS);
        for g in i..=end {
            if g > i {
                sql.push(',');
            }
            sql.push_str(&format!("({g},{})", (g * STRIDE) % ROWS));
        }
        eng.execute(&sql).expect("seed");
        i = end + 1;
    }
    // Rule 2: a timing read off an unverified table is not evidence. The
    // distinct count is the one that matters here — if it were below ROWS
    // the ablation would be measuring a different query than the sweep.
    let got = eng
        .execute("SELECT count(DISTINCT k) FROM t")
        .expect("count");
    let text = format!("{got:?}");
    assert!(
        text.contains(&ROWS.to_string()),
        "seed produced {text}, wanted {ROWS} distinct k — the probe would \
         otherwise time a DISTINCT that actually removes rows"
    );
}

/// Run through the STREAMING entry point, which is what pgwire uses.
///
/// `Engine::execute` materialises instead, and the two take different
/// executors — r1031 wired an ORDER BY lane into the streaming one and this
/// probe reported no change at all, because it was timing the other path.
/// An instrument that does not exercise the path under test reads exactly
/// like a change that did nothing.
fn run(eng: &mut Engine, label: &str, sql: &str, reps: u32) {
    let mut best = f64::MAX;
    let mut worst: f64 = 0.0;
    let a0 = ALLOCS.load(Relaxed);
    let b0 = ALLOC_BYTES.load(Relaxed);
    for _ in 0..reps {
        let spg_sql::ast::Statement::Select(stmt) =
            spg_sql::parser::parse_statement(sql).expect("parse")
        else {
            panic!("{label}: not a SELECT");
        };
        let mut rows = 0usize;
        let t0 = std::time::Instant::now();
        eng.execute_prepared_select_streaming(&stmt, spg_engine::CancelToken::none(), |item| {
            if matches!(item, spg_engine::StreamItem::Row(_)) {
                rows += 1;
            }
            Ok(())
        })
        .expect("query");
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        // A row count is the witness that the query ran: a lane that
        // declines silently and one that answers with nothing look the
        // same on the clock.
        assert_eq!(
            rows, ROWS as usize,
            "{label} returned {rows} rows, wanted {ROWS}"
        );
        best = best.min(ms);
        worst = worst.max(ms);
    }
    let allocs = (ALLOCS.load(Relaxed) - a0) / u64::from(reps);
    let bytes = (ALLOC_BYTES.load(Relaxed) - b0) / u64::from(reps);
    println!(
        "{label:<10} min {best:8.2} ms   max {worst:8.2} ms   \
         allocs/query {allocs:>9}   MB/query {:>6.1}   reps {reps}",
        bytes as f64 / 1_048_576.0
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "both".into());
    let reps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let mut eng = Engine::new();
    // `SPG_PROBE_SPILL=1` reproduces the server's configuration.
    if std::env::var("SPG_PROBE_SPILL").as_deref() == Ok("1") {
        eng.set_temp_run_factory(mem_run);
        assert!(eng.can_spill(), "the spill sink did not take");
        println!("(spill sink installed — the server's configuration)");
    }
    seed(&mut eng);
    // AFTER seeding: loading four hundred thousand rows allocates far more
    // than the queries do, and sampling it costs minutes and answers a
    // question nobody asked.
    init_sampling();

    let distinct = "SELECT DISTINCT k FROM t ORDER BY k";
    let plain = "SELECT k FROM t ORDER BY k";
    match which.as_str() {
        "distinct" => {
            run(&mut eng, "distinct", distinct, 1);
            if let Ok(mut v) = SAMPLES.lock() {
                v.clear();
            }
            run(&mut eng, "distinct", distinct, reps);
            report_samples("distinct");
        }
        "plain" => {
            run(&mut eng, "plain", plain, 1);
            if let Ok(mut v) = SAMPLES.lock() {
                v.clear();
            }
            run(&mut eng, "plain", plain, reps);
            report_samples("plain");
        }
        _ => {
            run(&mut eng, "plain", plain, reps);
            run(&mut eng, "distinct", distinct, reps);
            run(&mut eng, "plain", plain, reps);
            run(&mut eng, "distinct", distinct, reps);
        }
    }
}
