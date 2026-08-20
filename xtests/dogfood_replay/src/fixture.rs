//! Fixture descriptor parser.
//!
//! Each dogfood-replay fixture lives in its own directory under
//! `xtests/dogfood_replay/fixtures/`, with a `fixture.json` descriptor
//! that points at the snapshot tarball + the SQL + the expected
//! budgets. The on-disk layout is JSON (not TOML) so the framework
//! can stay tight on workspace lockfile deps — `serde_json` is
//! already locked, `toml` is not.
//!
//! The descriptor is split per fixture *kind*:
//!
//! - **`query`** — load a snapshot, then run one or more SQL
//!   statements with warm/p95 budgets.
//! - **`lock-hang-recovery`** — drive a synthetic crash-recovery
//!   scenario whose budget is total recovery wall time.
//! - **`wal-replay-bounded`** — synthesise a WAL with N delete
//!   records on an indexed table and measure replay wall time.
//!
//! The dispatcher in `main.rs` matches on `kind` and routes the
//! fixture to the matching subsystem.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level descriptor — one JSON file per fixture directory.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Fixture {
    /// Stable fixture name (matches the directory name, used as the
    /// CLI handle).
    pub name: String,

    /// ISO date the prod incident that motivated this fixture was
    /// filed. Informational; not load-bearing.
    pub filed: String,

    /// Customer / repo of origin (e.g. `mailrs`).
    pub source: String,

    /// Repo-relative path to the customer's filed report.
    /// Informational; left blank for synthetic fixtures.
    #[serde(default)]
    pub report: String,

    /// Was this filed off a *real* production incident? Synthetic /
    /// internal-only fixtures set `false` and are not gated by the
    /// `--production-only` flag.
    #[serde(default)]
    pub production: bool,

    /// Kind dispatches the framework to the right harness.
    pub kind: FixtureKind,
}

/// Per-kind discriminated union; only the matching field is populated.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum FixtureKind {
    /// Snapshot + queries.sql + budget.
    Query(QueryFixture),
    /// docker-stop-mid-checkpoint reproducer.
    LockHangRecovery(LockHangFixture),
    /// Synthesised WAL + bounded replay budget.
    WalReplayBounded(WalReplayFixture),
    /// v7.38.7 — a customer's real dump, killed mid-write and reopened.
    DumpCrashRecovery(DumpCrashFixture),
}

/// v7.38.7 (sentori, at their request) — restore a real dump, write
/// under it, `kill -9`, reopen, and check what survived.
///
/// What makes this different from the replay fixtures beside it: the
/// data is a customer's, taken after their own suite had written it,
/// and the check is SELF-VALIDATING rather than an expectation file.
/// Every assertion compares two answers the same database gave — an
/// index-backed query against a sequential scan of the same predicate —
/// so a rebuilt index that is subtly wrong fails without anyone having
/// recorded what right looked like. sentori named the two they would
/// watch: a GIN `jsonb_path_ops` and a BRIN, "the two we would least
/// expect a from-scratch engine to rebuild identically after an unclean
/// stop".
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DumpCrashFixture {
    /// gzipped `pg_dump` output, relative to the fixture dir.
    pub dump_gz: String,
    /// SHA-256 of the gzip, verified before anything is restored.
    pub sha256: String,
    /// Table → expected row count after a clean restore. A restore that
    /// silently drops rows must not reach the crash test looking healthy.
    pub restored_rows: Vec<(String, u64)>,
    /// Predicates that an index answers and a sequential scan can also
    /// answer. Each is run both ways after the reopen; they must agree.
    pub index_probes: Vec<IndexProbe>,
    /// How many rows to insert before the kill, and into what.
    pub write_burst: WriteBurst,
}

/// One index-vs-scan cross-check.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexProbe {
    /// What this probe is watching, for the failure message.
    pub what: String,
    /// The query as the planner would run it (index-eligible).
    pub indexed: String,
    /// The same question phrased so no index can answer it.
    pub scan: String,
}

/// The writes in flight when the process dies.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WriteBurst {
    /// Statement run in a loop; `{i}` is replaced by the iteration.
    pub statement: String,
    /// Kill after this many have been acknowledged.
    pub kill_after: u32,
    /// Table the burst writes into, for the post-reopen count.
    pub table: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryFixture {
    /// Tarball + sha256 of a prod-shape catalog.
    pub snapshot: Snapshot,
    /// One or more SQL files (path relative to the fixture dir).
    pub queries: Vec<QuerySet>,
    /// Per-query budget. A fixture passes only if *every* statement
    /// stays within budget.
    pub expected: QueryBudget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    /// Path to the tarball, relative to the fixture dir. Tarball
    /// itself is .gitignore'd — devs fetch via `url` or out-of-band.
    pub path: String,
    /// Optional public URL to fetch the tarball. Informational —
    /// the framework does not download (no `reqwest` dep here).
    #[serde(default)]
    pub url: String,
    /// SHA-256 hex of the tarball. Verified on load and via
    /// `verify` subcommand.
    pub sha256: String,
    /// Tarball size in bytes. Used as a sanity check during
    /// `verify` so corrupted half-downloads fail fast.
    #[serde(default)]
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuerySet {
    /// Path to a `.sql` file, relative to the fixture dir. Multiple
    /// `;`-separated statements are run as one workload.
    pub file: String,
    /// Discarded measurements before timing starts (lets the
    /// optimiser cache + page cache reach steady state).
    #[serde(default = "default_warmup")]
    pub warmup_iters: usize,
    /// Timed measurements that produce the percentile budget.
    #[serde(default = "default_measure")]
    pub measure_iters: usize,
}

fn default_warmup() -> usize {
    20
}

fn default_measure() -> usize {
    100
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryBudget {
    /// Max acceptable wall time for the *first* measurement
    /// (after warm-up; isolates "first cold tier touch after WAL
    /// replay" cases). 0 = unbounded.
    #[serde(default)]
    pub cold_ms_max: f64,
    /// Max acceptable median (`p50`) wall time across measured
    /// iters. 0 = unbounded.
    #[serde(default)]
    pub warm_ms_max: f64,
    /// Max acceptable `p95` wall time. 0 = unbounded.
    #[serde(default)]
    pub p95_ms_max: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockHangFixture {
    /// Path to the snapshot tarball to open before injecting the
    /// crash. Optional for purely synthesised reproducers.
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    /// Step list — the framework executes them in order, watching
    /// for the budget at the end.
    pub steps: Vec<RecoveryStep>,
    pub expected: RecoveryBudget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecoveryStep {
    /// Open a catalog at the given path (relative to the temp dir
    /// the snapshot was extracted into).
    OpenPath { catalog_path: String },
    /// Run one or more SQL statements (no timing).
    Execute { sql: String },
    /// Force a `kill -9` of the *current process* would lose
    /// in-memory state. Inside the framework we simulate this by
    /// dropping the `Database` handle without a clean checkpoint
    /// after the given delay (caller's job to inject mid-mutation
    /// state first).
    InjectKill9MidCheckpoint { delay_ms: u64 },
    /// Reopen the catalog and time the boot path (= WAL replay
    /// time). The reopen wall time feeds the budget.
    ReopenPath {
        catalog_path: String,
        #[serde(default = "default_true")]
        expect_success: bool,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecoveryBudget {
    /// Wall time from final `ReopenPath` start to ready.
    pub total_recovery_ms_max: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalReplayFixture {
    /// One-line summary of what the synthetic load reproduces.
    #[serde(default)]
    pub description: String,
    pub synthesise: SynthesiseSpec,
    pub expected: WalReplayBudget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SynthesiseSpec {
    /// Initial row count seeded before the WAL records are written.
    pub table_rows: usize,
    /// Secondary index count (the prod failure scaled with these).
    pub indices_count: usize,
    /// Sequence of WAL record batches to write before the replay
    /// is measured.
    pub wal_records: Vec<WalRecordBatch>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalRecordBatch {
    pub kind: String, // "delete" | "update" | "insert"
    pub count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalReplayBudget {
    pub replay_ms_max: u64,
}

/// Load and parse `<fixture_dir>/fixture.json`.
pub fn load_fixture(fixture_dir: &Path) -> Result<Fixture> {
    let path = fixture_dir.join("fixture.json");
    let body = fs::read_to_string(&path)
        .with_context(|| format!("read fixture.json at {}", path.display()))?;
    let fx: Fixture = serde_json::from_str(&body)
        .with_context(|| format!("parse fixture.json at {}", path.display()))?;
    if fx.name.is_empty() {
        return Err(anyhow!(
            "fixture.json at {} has empty `name`",
            path.display()
        ));
    }
    Ok(fx)
}

/// Discover every fixture directory under `fixtures/`. Returns
/// `(name, dir, fixture)` sorted by name.
pub fn discover_all(root: &Path) -> Result<Vec<(String, PathBuf, Fixture)>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in
        fs::read_dir(root).with_context(|| format!("read fixtures dir {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("fixture.json").exists() {
            // Skip non-fixture sub-directories (e.g. docs).
            continue;
        }
        let fx = load_fixture(&path)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_default();
        out.push((name, path, fx));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
