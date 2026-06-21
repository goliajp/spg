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
            FixtureKind::WalReplayBounded(_) => None,
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
    }
}

fn run_query_fixture(
    dir: &Path,
    fx: &Fixture,
    q: &fixture::QueryFixture,
    fast: bool,
) -> Result<Outcome> {
    let state = verify_snapshot(dir, &q.snapshot.path, &q.snapshot.sha256)?;
    let extracted = match state {
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
            extract_snapshot(&path).with_context(|| format!("extract snapshot for {}", fx.name))?
        }
    };

    let mut db = Database::open_path(&extracted.catalog_path)
        .map_err(ee)
        .with_context(|| format!("open_path {}", extracted.catalog_path.display()))?;

    let mut worst = Outcome::Pass;
    for qs in &q.queries {
        let sql_path = dir.join(&qs.file);
        let results = bench::bench_sql_file(&mut db, &sql_path, qs.warmup_iters, qs.measure_iters)?;
        for r in results {
            let cold_ok = q.expected.cold_ms_max == 0.0 || r.cold_ms <= q.expected.cold_ms_max;
            let warm_ok = q.expected.warm_ms_max == 0.0 || r.warm_p50_ms <= q.expected.warm_ms_max;
            let p95_ok = q.expected.p95_ms_max == 0.0 || r.warm_p95_ms <= q.expected.p95_ms_max;
            let ok = cold_ok && warm_ok && p95_ok;
            let tag = if ok { Outcome::Pass } else { Outcome::Fail };
            if tag == Outcome::Fail {
                worst = Outcome::Fail;
            }
            println!(
                "  {} cold={:.2}ms p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms (n={}) — budget cold≤{} p50≤{} p95≤{}",
                tag.tag(),
                r.cold_ms,
                r.warm_p50_ms,
                r.warm_p95_ms,
                r.warm_p99_ms,
                r.warm_max_ms,
                r.iters,
                q.expected.cold_ms_max,
                q.expected.warm_ms_max,
                q.expected.p95_ms_max,
            );
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
