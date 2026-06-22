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
    /// User address to bind in the query.
    #[arg(long, default_value = "lihao@golia.ai")]
    user: String,
    /// Mailbox name for Class B mailbox_id lookup.
    #[arg(long, default_value = "INBOX")]
    mailbox: String,
}

const CLASSES: &[&str] = &["track-a", "class-b", "class-c"];

fn sql_track_a(user: &str) -> String {
    format!(
        "SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
                MAX(m.internal_date) \
           FROM messages m \
           JOIN mailboxes mb ON m.mailbox_id = mb.id \
          WHERE mb.user_address = '{user}' \
            AND m.thread_id != '' \
          GROUP BY m.thread_id \
          ORDER BY MAX(m.internal_date) DESC \
          LIMIT 50"
    )
}

fn sql_class_b(user: &str, mailbox: &str) -> String {
    format!(
        "SELECT COUNT(*) FROM messages m \
          WHERE m.mailbox_id = ( \
                  SELECT id FROM mailboxes \
                   WHERE user_address = '{user}' AND name = '{mailbox}' \
                   LIMIT 1 \
                ) \
            AND (m.flags & 1) = 0 \
            AND NOT EXISTS ( \
                  SELECT 1 FROM email_analysis ea \
                   WHERE ea.message_id = m.id \
                     AND ea.category IN ('spam', 'scam') \
                )"
    )
}

fn sql_class_c(user: &str) -> String {
    format!(
        "WITH unread_threads AS ( \
           SELECT m.thread_id \
             FROM messages m \
             JOIN mailboxes mb ON m.mailbox_id = mb.id \
            WHERE mb.user_address = '{user}' \
              AND m.thread_id != '' \
              AND NOT EXISTS ( \
                    SELECT 1 FROM email_analysis ea \
                     WHERE ea.message_id = m.id \
                       AND ea.category IN ('spam', 'scam') \
                  ) \
            GROUP BY m.thread_id \
            HAVING BOOL_OR(m.archived) = false \
               AND COUNT(CASE WHEN (m.flags & 1) = 0 THEN 1 END) > 0 \
         ) \
         SELECT COUNT(*) FROM unread_threads"
    )
}

struct Workload {
    queries: Vec<(String, String)>,
}

impl Workload {
    fn new(user: &str, mailbox: &str) -> Self {
        Self {
            queries: vec![
                (CLASSES[0].into(), sql_track_a(user)),
                (CLASSES[1].into(), sql_class_b(user, mailbox)),
                (CLASSES[2].into(), sql_class_c(user)),
            ],
        }
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
    for c in CLASSES {
        per_class.insert((*c).to_string(), Vec::with_capacity(workers * iters));
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
        "{:>10}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "class", "n", "p50", "p90", "p95", "p99", "max"
    );
    let mut sorted_by_class: Vec<(String, Vec<u128>)> = per_class.to_vec();
    sorted_by_class.sort_by(|a, b| a.0.cmp(&b.0));
    for (class, samples) in &sorted_by_class {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        println!(
            "{:>10}  {:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
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

fn amplification_table(single: &[(String, Vec<u128>)], conc: &[(String, Vec<u128>)]) {
    println!("\n=== Amplification (concurrent p50 / single-thread p50) ===");
    println!(
        "{:>10}  {:>10}  {:>10}  {:>8}",
        "class", "single p50", "conc p50", "ratio"
    );
    for c in CLASSES {
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
            "{:>10}  {:>10}  {:>10}  {:>7.2}×",
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
    let workload = Arc::new(Workload::new(&args.user, &args.mailbox));

    let single = run_phase(&snapshot, &workload, 1, args.iters, args.warmup, "single");
    print_stats("Single-thread (workers=1)", &single);

    eprintln!(
        "\nspawning {} concurrent workers × {} iters × {} classes...",
        args.workers,
        args.iters,
        CLASSES.len()
    );
    let conc = run_phase(
        &snapshot,
        &workload,
        args.workers,
        args.iters,
        args.warmup,
        "concurrent",
    );
    print_stats(&format!("Concurrent (workers={})", args.workers), &conc);

    amplification_table(&single, &conc);

    // Brief pause to let any background allocator/cleanup quiesce.
    thread::sleep(Duration::from_millis(50));
    Ok(())
}
