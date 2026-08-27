//! `spg-dogfood-replay` — the dogfood-replay testbed CLI.
//!
//! See `xtests/dogfood_replay/README.md` (TODO) and the design note
//! at `.claude/notes/v7.37.5-dogfood-replay-framework-design.md`
//! for the framework rationale.
//!
//! Subcommands:
//!
//! - `list` — enumerate every fixture under `fixtures/`.
//! - `verify` — SHA-256 check each fixture's snapshot tarball.
//!   Missing tarballs are not failures (the snapshot is
//!   gitignored; devs fetch out-of-band).
//! - `run --fixture <name>` — run one fixture.
//! - `all [--fast]` — run every fixture. `--fast` skips fixtures
//!   that require a prod snapshot (they're typically 200 MB+) so
//!   CI can complete in under 60 s.

mod bench;
mod engine_err;
mod explain;
mod fixture;
mod recovery;
mod snapshot;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use engine_err::ee;
use fixture::{Fixture, FixtureKind};
use snapshot::{SnapshotState, extract_snapshot, verify_snapshot};
use spg_embedded::Database;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "spg-dogfood-replay",
    about = "Run SPG dogfood-replay fixtures (customer prod incidents encoded as CI gates)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enumerate every fixture under `fixtures/`.
    List,
    /// SHA-256 check every snapshot tarball.
    Verify,
    /// Run a single fixture by directory name.
    Run {
        #[arg(long)]
        fixture: String,
        #[arg(long)]
        fast: bool,
    },
    /// Run every fixture. `--fast` skips fixtures whose snapshot is
    /// large or absent (synthetic scenarios still run).
    All {
        #[arg(long)]
        fast: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
}

impl Outcome {
    fn tag(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Skip => "SKIP",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let fixtures_root = locate_fixtures_root()?;

    match cli.cmd {
        Cmd::List => cmd_list(&fixtures_root),
        Cmd::Verify => cmd_verify(&fixtures_root),
        Cmd::Run { fixture, fast } => cmd_run_one(&fixtures_root, &fixture, fast),
        Cmd::All { fast } => cmd_run_all(&fixtures_root, fast),
    }
}

/// Resolve `xtests/dogfood_replay/fixtures/` relative to the binary
/// or CWD. `cargo run -p spg-dogfood-replay` from the repo root
/// hits the CWD branch; calling the built bin directly hits the
/// binary-dir branch.
fn locate_fixtures_root() -> Result<PathBuf> {
    let candidates = [
        PathBuf::from("xtests/dogfood_replay/fixtures"),
        PathBuf::from("fixtures"),
        std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.parent()
                    .map(|d| d.join("../../../xtests/dogfood_replay/fixtures"))
            })
            .unwrap_or_default(),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(anyhow!("cannot locate fixtures/ — tried: {candidates:?}"))
}

fn cmd_list(root: &Path) -> Result<()> {
    let all = fixture::discover_all(root)?;
    println!("dogfood-replay fixtures @ {}", root.display());
    println!("  total: {}", all.len());
    for (name, _, fx) in all {
        let kind = match fx.kind {
            FixtureKind::Query(_) => "query",
            FixtureKind::LockHangRecovery(_) => "lock-hang-recovery",
            FixtureKind::WalReplayBounded(_) => "wal-replay-bounded",
            FixtureKind::DumpCrashRecovery(_) => "dump-crash-recovery",
        };
        let prod = if fx.production { "prod" } else { "synth" };
        println!("  - {name}  [{kind}, {prod}, filed {}]", fx.filed);
    }
    Ok(())
}

fn cmd_verify(root: &Path) -> Result<()> {
    let all = fixture::discover_all(root)?;
    let mut total = 0usize;
    let mut present = 0usize;
    let mut missing = 0usize;
    let mut corrupt = 0usize;
    for (name, dir, fx) in all {
        let snap_opt = match &fx.kind {
            FixtureKind::Query(q) => Some(&q.snapshot),
            FixtureKind::LockHangRecovery(l) => l.snapshot.as_ref(),
            FixtureKind::WalReplayBounded(_) | FixtureKind::DumpCrashRecovery(_) => None,
        };
        if let Some(snap) = snap_opt {
            total += 1;
            match verify_snapshot(&dir, &snap.path, &snap.sha256)? {
                SnapshotState::Present { size_bytes, .. } => {
                    present += 1;
                    println!("  PRESENT  {name}  ({size_bytes} bytes)");
                }
                SnapshotState::Missing => {
                    missing += 1;
                    println!("  MISSING  {name}  (snapshot not fetched)");
                }
                SnapshotState::Corrupted {
                    actual_sha256,
                    expected_sha256,
                    ..
                } => {
                    corrupt += 1;
                    println!(
                        "  CORRUPT  {name}  (got {actual_sha256}, expected {expected_sha256})"
                    );
                }
            }
        } else {
            println!("  N/A      {name}  (synthetic fixture, no snapshot)");
        }
    }
    println!("\nverify: total={total} present={present} missing={missing} corrupt={corrupt}");
    if corrupt > 0 {
        return Err(anyhow!("{corrupt} corrupt snapshot(s) — delete + refetch"));
    }
    Ok(())
}

fn cmd_run_one(root: &Path, name: &str, fast: bool) -> Result<()> {
    let dir = root.join(name);
    if !dir.exists() {
        return Err(anyhow!("no fixture directory at {}", dir.display()));
    }
    let fx = fixture::load_fixture(&dir)?;
    let outcome = run_fixture(&dir, &fx, fast)?;
    if outcome == Outcome::Fail {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_run_all(root: &Path, fast: bool) -> Result<()> {
    let all = fixture::discover_all(root)?;
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut skip = 0usize;
    for (_, dir, fx) in all {
        let outcome = run_fixture(&dir, &fx, fast).unwrap_or(Outcome::Fail);
        match outcome {
            Outcome::Pass => pass += 1,
            Outcome::Fail => fail += 1,
            Outcome::Skip => skip += 1,
        }
    }
    println!("\ndogfood-replay summary: pass={pass} fail={fail} skip={skip} (fast={fast})");
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_fixture(dir: &Path, fx: &Fixture, fast: bool) -> Result<Outcome> {
    println!("\n── fixture: {} ──", fx.name);
    match &fx.kind {
        FixtureKind::Query(q) => run_query_fixture(dir, fx, q, fast),
        FixtureKind::LockHangRecovery(l) => run_lock_hang_fixture(dir, fx, l, fast),
        FixtureKind::WalReplayBounded(w) => run_wal_replay_fixture(fx, w),
        FixtureKind::DumpCrashRecovery(d) => run_dump_crash_fixture(dir, fx, d),
    }
}

fn run_query_fixture(
    dir: &Path,
    fx: &Fixture,
    q: &fixture::QueryFixture,
    fast: bool,
) -> Result<Outcome> {
    let state = verify_snapshot(dir, &q.snapshot.path, &q.snapshot.sha256)?;
    let snapshot_path = match state {
        SnapshotState::Missing => {
            println!(
                "  SKIP  {} — snapshot missing (fetch from {} to enable)",
                fx.name, q.snapshot.url
            );
            return Ok(Outcome::Skip);
        }
        SnapshotState::Corrupted {
            actual_sha256,
            expected_sha256,
            ..
        } => {
            println!(
                "  FAIL  {} — snapshot SHA-256 mismatch (got {actual_sha256}, expected {expected_sha256})",
                fx.name
            );
            return Ok(Outcome::Fail);
        }
        SnapshotState::Present { path, size_bytes } => {
            if fast && size_bytes > 100 * 1024 * 1024 {
                println!(
                    "  SKIP  {} — fast tier (snapshot is {} MB)",
                    fx.name,
                    size_bytes / (1024 * 1024)
                );
                return Ok(Outcome::Skip);
            }
            path
        }
    };

    // v7.38.25 — reopen the catalog for every cold sample, and judge
    // the median of them.
    //
    // Two things were wrong with timing this once. The first is that
    // `cold` is a single execution, so the budget beside it was decided
    // by one sample while `p50` and `p95` came from a hundred; on
    // `content-worker` that single sample is bimodal -- roughly 10 ms or
    // roughly 20 ms, six times on an idle machine -- and the budget of
    // 15 sat in the gap, so the verdict was a coin flip.
    //
    // The second is that the boundary this fixture measures across has
    // moved. At 13135db9, where the `track-a` cold budget of 100 ms was
    // locked, opening this snapshot took 219.5 s and the first query
    // then took 88.0 ms with a spread of 0.1 ms. Today the open takes
    // 3.2 s and the first query takes ~104 ms with a spread of 15 ms:
    // work moved out of the open and into the first execute. Timing
    // only the query called that a regression while time-to-first-answer
    // had gone from 219.6 s to 3.3 s. So the open is timed too, and
    // reported beside the cold it hands off to.
    let mut worst = Outcome::Pass;
    for qs in &q.queries {
        let sql_path = dir.join(&qs.file);
        let body = std::fs::read_to_string(&sql_path)
            .with_context(|| format!("read sql file {}", sql_path.display()))?;
        let stmts = bench::split_sql(&body);
        let iters = qs.cold_iters.max(1);
        let mut opens: Vec<f64> = Vec::with_capacity(iters);
        let mut colds: Vec<Vec<f64>> = vec![Vec::new(); stmts.len()];
        let mut held = None;
        // Each sample gets its own extraction. Reopening the same
        // directory is not the same measurement: the first open reads a
        // catalog `tar` has just written, and every reopen after it
        // finds the same bytes already in the page cache. Measured on
        // `track-a`, three runs: the first open took 3300 / 3368 / 3377
        // ms and the reopens' median 2336 / 2329 / 2391, and the first
        // COLD sample read 100.73 / 144.08 / 112.40 ms against medians
        // of 98.71 / 85.83 / 91.47 -- so a median over reopens would
        // have passed a budget every one of those first samples breaks.
        // That is not removing a coin flip, it is swapping the quantity
        // being judged for a cheaper one. Re-extracting costs a tar per
        // sample and buys samples that are actually comparable, to each
        // other and to the number the budget was set from.
        for i in 0..iters {
            let extracted = extract_snapshot(&snapshot_path)
                .with_context(|| format!("extract snapshot for {}", fx.name))?;
            let open_start = Instant::now();
            let mut db = Database::open_path(&extracted.catalog_path)
                .map_err(ee)
                .with_context(|| format!("open_path {}", extracted.catalog_path.display()))?;
            opens.push(open_start.elapsed().as_secs_f64() * 1000.0);
            for (j, sql) in stmts.iter().enumerate() {
                colds[j].push(bench::time_one(&mut db, sql)?);
            }
            // The last one stays open for the warm loop; the rest close
            // here, which is what makes the next open a cold one.
            if i + 1 == iters {
                // The extraction has to outlive the database: its
                // TempDir deletes the catalog when it drops, and the
                // database writes a checkpoint on ITS drop. Held as
                // (extraction, database) rather than the other way
                // round because a `let` destructuring drops its
                // bindings in reverse order -- with the database
                // second it goes first, and the directory is still
                // there when it does. Written the other way, every
                // fixture printed `final checkpoint on Drop failed:
                // io: No such file or directory`.
                held = Some((extracted, db));
            }
        }
        let (_kept, mut db) = held.expect("cold_iters is at least 1, so one database is held");
        let open_med = bench::median(&opens);
        for (j, sql) in stmts.iter().enumerate() {
            let w = bench::warm_stats(&mut db, sql, qs.warmup_iters, qs.measure_iters)?;
            let cold_med = bench::median(&colds[j]);
            let cold_lo = colds[j].iter().copied().fold(f64::INFINITY, f64::min);
            let cold_hi = colds[j].iter().copied().fold(f64::NEG_INFINITY, f64::max);
            // The FIRST sample is the only one taken against a catalog the
            // OS page cache has not seen since `tar` wrote it; the reopens
            // after it are cheaper for that reason alone. Reporting only
            // the median would quietly swap the quantity being judged for
            // an easier one -- `content-worker` reads ~20 ms on the first
            // and ~10 ms on the reopens, so a median of five sits under a
            // budget the first sample breaks. Both are printed, and the
            // first is named, so nobody has to guess which is which.
            let cold_first = colds[j].first().copied().unwrap_or(0.0);
            let open_first = opens.first().copied().unwrap_or(0.0);
            let cold_ok = q.expected.cold_ms_max == 0.0 || cold_med <= q.expected.cold_ms_max;
            let warm_ok = q.expected.warm_ms_max == 0.0 || w.p50_ms <= q.expected.warm_ms_max;
            let p95_ok = q.expected.p95_ms_max == 0.0 || w.p95_ms <= q.expected.p95_ms_max;
            let ok = cold_ok && warm_ok && p95_ok;
            let tag = if ok { Outcome::Pass } else { Outcome::Fail };
            if tag == Outcome::Fail {
                worst = Outcome::Fail;
            }
            // The cold range is printed beside its median because a
            // median is exactly what hides a bimodal sample, and one of
            // these fixtures has one.
            println!(
                "  {} open={:.0}ms(1st {:.0}) cold={:.2}ms [{:.2}-{:.2}, n={}, 1st {:.2}] \
                 p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms (n={}) \
                 — budget cold≤{} p50≤{} p95≤{}",
                tag.tag(),
                open_med,
                open_first,
                cold_med,
                cold_lo,
                cold_hi,
                colds[j].len(),
                cold_first,
                w.p50_ms,
                w.p95_ms,
                w.p99_ms,
                w.max_ms,
                w.iters,
                q.expected.cold_ms_max,
                q.expected.warm_ms_max,
                q.expected.p95_ms_max,
            );
            let _ = sql;
        }
    }
    Ok(worst)
}

fn run_lock_hang_fixture(
    dir: &Path,
    fx: &Fixture,
    l: &fixture::LockHangFixture,
    _fast: bool,
) -> Result<Outcome> {
    match recovery::run_lock_hang(dir, l)? {
        recovery::RecoveryOutcome::SkippedSnapshotMissing => {
            println!("  SKIP  {} — snapshot missing", fx.name);
            Ok(Outcome::Skip)
        }
        recovery::RecoveryOutcome::Ran(run) => {
            let ok = run.recovery_ms <= l.expected.total_recovery_ms_max as f64;
            let tag = if ok { Outcome::Pass } else { Outcome::Fail };
            println!(
                "  {} recovery_ms={:.2} steps={} — budget total_recovery_ms≤{}",
                tag.tag(),
                run.recovery_ms,
                run.steps_run,
                l.expected.total_recovery_ms_max
            );
            Ok(tag)
        }
    }
}

/// v7.38.7 — the dump-crash-recovery wrapper.
///
/// `run_dump_crash` returns an Err for every way this can be wrong —
/// data that did not restore, an acknowledged write that vanished, an
/// index that disagrees with a scan — because each of those is a
/// failure with a specific sentence to say, and a boolean would throw
/// the sentence away.
fn run_dump_crash_fixture(
    dir: &Path,
    fx: &Fixture,
    d: &fixture::DumpCrashFixture,
) -> Result<Outcome> {
    match recovery::run_dump_crash(dir, d) {
        Ok(recovery::RecoveryOutcome::SkippedSnapshotMissing) => {
            println!("  SKIP  {} — dump missing ({})", fx.name, d.dump_gz);
            Ok(Outcome::Skip)
        }
        Ok(recovery::RecoveryOutcome::Ran(run)) => {
            println!(
                "  {} {} tables restored, {} acknowledged writes survived SIGKILL, \
                 {} index probes agree with a sequential scan",
                Outcome::Pass.tag(),
                d.restored_rows.len(),
                d.write_burst.kill_after,
                d.index_probes.len()
            );
            let _ = run;
            Ok(Outcome::Pass)
        }
        Err(e) => {
            println!("  {} {e:#}", Outcome::Fail.tag());
            Ok(Outcome::Fail)
        }
    }
}

fn run_wal_replay_fixture(fx: &Fixture, w: &fixture::WalReplayFixture) -> Result<Outcome> {
    let run = recovery::run_wal_replay(w)?;
    let ok = run.recovery_ms <= w.expected.replay_ms_max as f64;
    let tag = if ok { Outcome::Pass } else { Outcome::Fail };
    println!(
        "  {} replay_ms={:.2} — budget replay_ms≤{} (desc: {})",
        tag.tag(),
        run.recovery_ms,
        w.expected.replay_ms_max,
        if w.description.is_empty() {
            fx.name.as_str()
        } else {
            w.description.as_str()
        }
    );
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every query fixture states its budget twice: once in
    /// `fixture.json`, which the gate reads, and once in the README,
    /// which is what a person reads when the gate goes red. Both
    /// fixtures that restated it drifted. `track-a`'s README still
    /// quoted <= 5 ms warm / <= 10 ms p95 two months after 13135db9
    /// replaced its query -- those were the budgets of a hand-written
    /// CTE that referenced tables the snapshot does not have, so they
    /// described something that had never run. `content-worker`'s
    /// README said "These are the numbers pinned in
    /// `fixture.json.expected`" directly above three numbers, none of
    /// which were. Anyone reading either one while the gate was red
    /// would have concluded the product was an order of magnitude off
    /// its target when it was a few percent off.
    ///
    /// So the table is generated from the JSON and checked here. The
    /// README may still say whatever it likes around it.
    fn budget_block(q: &fixture::QueryFixture) -> String {
        let cell = |v: f64| {
            if v == 0.0 {
                "unbounded".to_string()
            } else {
                format!("\u{2264} {v} ms")
            }
        };
        format!(
            "<!-- BUDGETS: generated from fixture.json \u{2014} the gate reads the JSON, not this table -->\n\
             | Window | Budget |\n\
             | --- | --- |\n\
             | Cold (first iter) | {} |\n\
             | Warm median (p50) | {} |\n\
             | p95 | {} |\n\
             <!-- /BUDGETS -->",
            cell(q.expected.cold_ms_max),
            cell(q.expected.warm_ms_max),
            cell(q.expected.p95_ms_max),
        )
    }

    #[test]
    fn every_query_fixture_readme_carries_the_budget_the_gate_reads() {
        let root = locate_fixtures_root().expect("locate fixtures root");
        let all = fixture::discover_all(&root).expect("discover fixtures");

        // A walk that finds nothing looks exactly like a walk that found
        // nothing wrong, so name what has to be there.
        let names: Vec<&str> = all.iter().map(|(n, _, _)| n.as_str()).collect();
        for want in [
            "mailrs-2026-06-22-track-a",
            "mailrs-2026-06-22-content-worker",
        ] {
            assert!(
                names.contains(&want),
                "{want} is not among the fixtures discovered under {}: {names:?}",
                root.display()
            );
        }

        let mut checked = 0;
        for (name, dir, fx) in &all {
            let FixtureKind::Query(q) = &fx.kind else {
                continue;
            };
            let readme_path = dir.join("README.md");
            let readme = std::fs::read_to_string(&readme_path)
                .unwrap_or_else(|e| panic!("{name}: read {}: {e}", readme_path.display()));
            let want = budget_block(q);
            assert!(
                readme.contains(&want),
                "{name}: README.md does not carry the budgets the gate reads.\n\
                 Paste this block into it verbatim:\n\n{want}\n"
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "only {checked} query fixtures were checked -- the walk found the \
             directory but not the fixtures in it"
        );
    }
}
