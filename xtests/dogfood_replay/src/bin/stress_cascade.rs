//! v7.37.7 — concurrent stress harness to reproduce the mailrs prod cascade.
//!
//! Every prior cycle (06-16 / 06-17 / 06-18 / 06-19 / 06-20 / 06-22) the
//! SPG-side measurement has been single-query bench-style: open Database,
//! run the SQL once / N times serially, declare a number. Mailrs prod
//! reports 50× amplification under 20-connection concurrent load. This
//! harness fills that blind spot: open the prod snapshot ONCE, clone the
//! `CatalogSnapshot` to N worker threads, run the cascade SQL classes
//! concurrently, report per-class latency under load vs single-thread.
//!
//! Architecturally:
//! - `Engine::clone_snapshot()` returns a `CatalogSnapshot` explicitly
//!   designed for cross-thread concurrent reads
//!   (`Engine::execute_readonly_on_snapshot` takes `&CatalogSnapshot`,
//!   not `&self`).
//! - This is the same pattern `spg-server` uses for read traffic — every
//!   incoming SELECT takes a snapshot.
//! - If under-load latency >> single-thread baseline, the bottleneck is
//!   contention inside the engine (lock / cold-tier / allocator), not
//!   per-query planning. That is the data we need to direct the v7.37.7
//!   attack.
//!
//! Usage:
//!     cargo run --release -p spg-dogfood-replay --bin spg-stress-cascade -- \
//!         --snapshot xtests/dogfood_replay/fixtures/mailrs-2026-06-22-track-a/snapshot.tar.gz \
//!         --workers 20 --iters 50

use anyhow::{Context, Result, bail};
use clap::Parser;
use spg_embedded::Database;
use spg_engine::Engine;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Parser, Debug)]
#[command(
    about = "Concurrent stress harness for mailrs cascade SQL on a prod snapshot."
)]
struct Args {
    /// Path to the snapshot tarball (e.g. one of the mailrs fixtures).
    #[arg(long)]
    snapshot: PathBuf,
    /// Concurrent worker thread count (mailrs prod = 20).
    #[arg(long, default_value_t = 20)]
    workers: usize,
    /// Iterations per worker per query class.
    #[arg(long, default_value_t = 50)]
    iters: usize,
    /// Warmup iters per worker per class (discarded from stats).
    #[arg(long, default_value_t = 5)]
    warmup: usize,
    /// Fixture directory root (each subdir has `queries.sql` read verbatim).
    /// v7.37.7 fidelity fix — earlier versions inlined a simplified
    /// 4-col Track A SQL; harness reported 4 ms while the real 167-col
    /// fixture is 76 ms. Now both are aligned by reading the fixture
    /// corpus directly.
    #[arg(long, default_value = "xtests/dogfood_replay/fixtures")]
    fixtures_root: PathBuf,
    /// Fixture names to load (each must have <root>/<name>/queries.sql).
    /// Comma-separated. Default matches the mailrs cascade trio.
    #[arg(
        long,
        default_value = "mailrs-2026-06-22-track-a,mailrs-2026-06-22-class-b-unread-not-exists,mailrs-2026-06-22-class-c-dashboard-cte"
    )]
    fixtures: String,
}

/// Strip `--` line comments and split on `;` — matches
/// `dogfood_replay::bench::split_sql` so the stress binary executes
/// the same statements the single-shot `run` subcommand does.
fn load_fixture_sql(fixtures_root: &Path, fixture_name: &str) -> Result<String> {
    let path = fixtures_root.join(fixture_name).join("queries.sql");
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("read fixture queries.sql at {}", path.display()))?;
    // Take the first non-empty statement after splitting by `;` and
    // stripping `--` line comments. Matches `bench::split_sql`.
    for raw in body.split(';') {
        let cleaned: String = raw
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    bail!(
        "fixture {} has no non-empty SQL statement after comment strip",
        path.display()
    )
}

/// Short tag used in the per-class summary output. Strips the
/// `mailrs-YYYY-MM-DD-` prefix when present so the table fits.
fn short_label(fixture_name: &str) -> String {
    fixture_name
        .strip_prefix("mailrs-")
        .and_then(|s| {
            // After "mailrs-", the next segment is the date "YYYY-MM-DD"
            // (10 chars). Skip it + the separating dash.
            s.get(11..)
        })
        .unwrap_or(fixture_name)
        .to_string()
}

struct Workload {
    queries: Vec<(String, String)>,
}

impl Workload {
    fn load(args: &Args) -> Result<Self> {
        let names: Vec<String> = args
            .fixtures
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            bail!("--fixtures argument resolved to zero names");
        }
        let mut queries: Vec<(String, String)> = Vec::new();
        for name in &names {
            let sql = load_fixture_sql(&args.fixtures_root, name)?;
            queries.push((short_label(name), sql));
        }
        Ok(Self { queries })
    }
    fn labels(&self) -> Vec<String> {
        self.queries.iter().map(|(c, _)| c.clone()).collect()
    }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn fmt_us(us: u128) -> String {
    if us >= 1_000_000 {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.2}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}us")
    }
}

fn locate_catalog(root: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(root).context("read snapshot temp dir")? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.ends_with(".spg")
                && !name.ends_with(".spg.wal")
                && !name.ends_with(".spg.lock")
            {
                return Ok(path);
            }
        }
    }
    bail!("no *.spg catalog directory under {}", root.display())
}

fn extract(tarball: &Path) -> Result<(TempDir, PathBuf)> {
    let tmp = tempfile::Builder::new()
        .prefix("spg-stress-")
        .tempdir()
        .context("temp dir for snapshot extract")?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(tmp.path())
        .status()
        .context("spawn `tar -xzf` for snapshot")?;
    if !status.success() {
        bail!(
            "tar -xzf {} -C {} failed with exit {:?}",
            tarball.display(),
            tmp.path().display(),
            status.code()
        );
    }
    let catalog = locate_catalog(tmp.path())?;
    // Drop any stale lock dir from the prod kill-9 state.
    let lock_dir = format!("{}.lock", catalog.display());
    let _ = std::fs::remove_dir_all(&lock_dir);
    Ok((tmp, catalog))
}

fn run_phase(
    snapshot: &Arc<spg_engine::CatalogSnapshot>,
    workload: &Arc<Workload>,
    workers: usize,
    iters: usize,
    warmup: usize,
    label: &str,
) -> Vec<(String, Vec<u128>)> {
    let mut per_class: std::collections::HashMap<String, Vec<u128>> =
        std::collections::HashMap::new();
    for c in workload.labels() {
        per_class.insert(c, Vec::with_capacity(workers * iters));
    }
    let barrier = Arc::new(Barrier::new(workers));
    let handles: Vec<_> = (0..workers)
        .map(|wi| {
            let snap = Arc::clone(snapshot);
            let wl = Arc::clone(workload);
            let bar = Arc::clone(&barrier);
            thread::spawn(move || {
                // Warmup — discarded.
                for _ in 0..warmup {
                    for (_, sql) in &wl.queries {
                        let _ = Engine::execute_readonly_on_snapshot(&snap, sql);
                    }
                }
                bar.wait();
                let mut samples: Vec<(String, u128)> =
                    Vec::with_capacity(iters * wl.queries.len());
                for j in 0..iters {
                    let start_offset = wi.wrapping_add(j) % wl.queries.len();
                    for k in 0..wl.queries.len() {
                        let idx = (start_offset + k) % wl.queries.len();
                        let (class, sql) = &wl.queries[idx];
                        let t = Instant::now();
                        let _ = Engine::execute_readonly_on_snapshot(&snap, sql);
                        let us = t.elapsed().as_micros();
                        samples.push((class.clone(), us));
                    }
                }
                samples
            })
        })
        .collect();
    for h in handles {
        let samples = h.join().expect("worker panicked");
        for (class, us) in samples {
            per_class.get_mut(&class).unwrap().push(us);
        }
    }
    eprintln!("[{label}] all workers joined ({workers} × {iters} iters)");
    per_class.into_iter().collect()
}

fn print_stats(label: &str, per_class: &[(String, Vec<u128>)]) {
    println!("\n=== {label} ===");
    println!(
        "{:>32}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "class", "n", "p50", "p90", "p95", "p99", "max"
    );
    let mut sorted_by_class: Vec<(String, Vec<u128>)> = per_class.to_vec();
    sorted_by_class.sort_by(|a, b| a.0.cmp(&b.0));
    for (class, samples) in &sorted_by_class {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        println!(
            "{:>32}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
            class,
            n,
            fmt_us(percentile(&sorted, 0.50)),
            fmt_us(percentile(&sorted, 0.90)),
            fmt_us(percentile(&sorted, 0.95)),
            fmt_us(percentile(&sorted, 0.99)),
            fmt_us(*sorted.last().unwrap_or(&0))
        );
    }
}

fn lookup_class<'a>(stats: &'a [(String, Vec<u128>)], class: &str) -> Option<&'a [u128]> {
    stats
        .iter()
        .find(|(c, _)| c == class)
        .map(|(_, v)| v.as_slice())
}

fn amplification_table(
    labels: &[String],
    single: &[(String, Vec<u128>)],
    conc: &[(String, Vec<u128>)],
) {
    println!("\n=== Amplification (concurrent p50 / single-thread p50) ===");
    println!(
        "{:>32}  {:>10}  {:>10}  {:>8}",
        "class", "single p50", "conc p50", "ratio"
    );
    for c in labels {
        let s = lookup_class(single, c).unwrap_or(&[]);
        let cv = lookup_class(conc, c).unwrap_or(&[]);
        if s.is_empty() || cv.is_empty() {
            continue;
        }
        let mut s_sorted = s.to_vec();
        s_sorted.sort_unstable();
        let mut c_sorted = cv.to_vec();
        c_sorted.sort_unstable();
        let s50 = percentile(&s_sorted, 0.50);
        let c50 = percentile(&c_sorted, 0.50);
        let ratio = if s50 == 0 {
            0.0
        } else {
            c50 as f64 / s50 as f64
        };
        println!(
            "{:>32}  {:>10}  {:>10}  {:>7.2}×",
            c,
            fmt_us(s50),
            fmt_us(c50),
            ratio
        );
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    eprintln!("snapshot: {}", args.snapshot.display());

    let t = Instant::now();
    let (_tmp, catalog) = extract(&args.snapshot)?;
    eprintln!(
        "extracted in {:.2}s — catalog at {}",
        t.elapsed().as_secs_f64(),
        catalog.display()
    );

    let t = Instant::now();
    let db = Database::open_path(&catalog)
        .map_err(|e| anyhow::anyhow!("engine error: {e}"))
        .context("open Database from snapshot path")?;
    eprintln!("Database::open_path in {:.2}s", t.elapsed().as_secs_f64());

    let snapshot = Arc::new(db.engine().clone_snapshot());
    let workload = Arc::new(Workload::load(&args)?);
    eprintln!("loaded {} fixture queries:", workload.queries.len());
    for (name, sql) in &workload.queries {
        let preview: String = sql.chars().take(80).collect();
        eprintln!("  - {name:<32} {preview}…");
    }
    let labels = workload.labels();

    // v7.37.7 round-2 contention instrumentation — reset
    // `MemoizeCache::counters` between phases to attribute new()/put()/drop
    // counts strictly to the timed runs (warmup excluded).
    spg_engine::memoize::counters::reset();
    let pullup_before_single =
        spg_engine::EXISTS_PULLUP_FIRE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    let single = run_phase(&snapshot, &workload, 1, args.iters, args.warmup, "single");
    let single_counters = spg_engine::memoize::counters::snapshot();
    let pullup_single = spg_engine::EXISTS_PULLUP_FIRE_COUNT
        .load(std::sync::atomic::Ordering::Relaxed)
        - pullup_before_single;
    print_stats("Single-thread (workers=1)", &single);
    println!(
        "  [memoize counters / single] new={} put={} max_entries_seen={} drop_empty={} drop_with_entries={}",
        single_counters.new_calls,
        single_counters.put_calls,
        single_counters.max_entries_seen,
        single_counters.drop_with_zero_entries,
        single_counters.drop_with_entries
    );
    println!(
        "  [pullup / single] candidate={} fire={} bail_inner_shape={} bail_inner_from={} bail_no_where={} bail_residual_not_inner={} bail_no_corr={} bail_multicol_disabled={} bail_unique_key_missing={}",
        spg_engine::EXISTS_PULLUP_CANDIDATE_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        pullup_single,
        spg_engine::EXISTS_PULLUP_BAIL_INNER_SHAPE.load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_INNER_FROM.load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_NO_WHERE.load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER
            .load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_NO_CORR.load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_MULTICOL_DISABLED
            .load(std::sync::atomic::Ordering::Relaxed),
        spg_engine::EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING
            .load(std::sync::atomic::Ordering::Relaxed),
    );

    eprintln!(
        "\nspawning {} concurrent workers × {} iters × {} classes...",
        args.workers,
        args.iters,
        labels.len()
    );
    spg_engine::memoize::counters::reset();
    let conc = run_phase(
        &snapshot,
        &workload,
        args.workers,
        args.iters,
        args.warmup,
        "concurrent",
    );
    let conc_counters = spg_engine::memoize::counters::snapshot();
    print_stats(&format!("Concurrent (workers={})", args.workers), &conc);
    println!(
        "  [memoize counters / concurrent] new={} put={} max_entries_seen={} drop_empty={} drop_with_entries={}",
        conc_counters.new_calls,
        conc_counters.put_calls,
        conc_counters.max_entries_seen,
        conc_counters.drop_with_zero_entries,
        conc_counters.drop_with_entries
    );

    amplification_table(&labels, &single, &conc);

    // Brief pause to let any background allocator/cleanup quiesce.
    thread::sleep(Duration::from_millis(50));
    Ok(())
}
