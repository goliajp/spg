//! Corpus discovery + per-permutation execution.
//!
//! Two phases:
//!
//!   1. `discover_corpus` — walk the corpus_root, collect every `.test` file
//!      under the include_globs (matched as subdir name prefixes).
//!
//!   2. `run_permutation` — for an `embedded` permutation, instantiate a
//!      fresh `sqllogictest::Runner` per fixture (mirrors the existing
//!      `sqllogictest` bin's per-file isolation) and tally pass/fail.
//!      For `server` permutations we return a structured `Skipped` because
//!      the spg-server bridge is on the v7.38 day-6 work-list; the
//!      skeleton parses + dispatches them so a follow-up commit can fill
//!      in `ServerRunner` without touching the CLI surface.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sqllogictest::{Runner, parser};

use crate::permutation::{Mode, PermFile, Permutation};

#[derive(Debug, Clone)]
pub struct FixtureResult {
    pub fixture: PathBuf,
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    /// First 3 failure messages, truncated. Full output stays in stdout.
    pub fail_snippets: Vec<String>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone)]
pub struct PermReport {
    pub permutation: String,
    pub status: PermStatus,
    pub fixtures: Vec<FixtureResult>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone)]
pub enum PermStatus {
    Ran,
    /// Mode not yet wired in the skeleton; the CLI returns success so a
    /// `--fast` run doesn't fail the gate. Filled in by a follow-up commit
    /// that brings up the spg-server bridge.
    SkippedPending(String),
}

impl PermReport {
    pub fn totals(&self) -> (usize, usize, usize) {
        let mut p = 0;
        let mut f = 0;
        let mut s = 0;
        for fr in &self.fixtures {
            p += fr.pass;
            f += fr.fail;
            s += fr.skip;
        }
        (p, f, s)
    }
}

pub fn discover_corpus(workspace_root: &Path, file: &PermFile) -> Result<Vec<PathBuf>, String> {
    let root = workspace_root.join(&file.default.corpus_root);
    if !root.is_dir() {
        return Err(format!("corpus_root not a directory: {}", root.display()));
    }
    let mut out = Vec::new();
    for include in &file.default.include_globs {
        let sub = root.join(include);
        if !sub.is_dir() {
            // Tolerate a missing subdir — keeps the skeleton from blowing
            // up when the corpus hasn't been seeded yet. Reported by the
            // CLI's `verify` command instead.
            continue;
        }
        walk_tests(&sub, &mut out)?;
    }
    out.sort();
    Ok(out)
}

pub fn discover_sample(workspace_root: &Path, file: &PermFile) -> Result<Vec<PathBuf>, String> {
    let sample = workspace_root
        .join(&file.default.corpus_root)
        .join(&file.default.fast_tier_sample);
    if !sample.is_dir() {
        return Err(format!(
            "fast_tier_sample not a directory: {}",
            sample.display()
        ));
    }
    let mut out = Vec::new();
    walk_tests(&sample, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_tests(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            walk_tests(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("test") {
            out.push(path);
        }
    }
    Ok(())
}

pub fn run_permutation(perm: &Permutation, fixtures: &[PathBuf]) -> PermReport {
    let started = Instant::now();
    let mut report = PermReport {
        permutation: perm.name.clone(),
        status: PermStatus::Ran,
        fixtures: Vec::new(),
        duration_ms: 0,
    };

    match perm.mode {
        Mode::Embedded => {
            // Permutation env-vars are set into the *current* process so
            // the embedded engine — which reads `SPG_TEST_DISABLE_*` GUC
            // shims at construction time (element A) — sees them. The
            // parent process running `all` forks a child per permutation,
            // so env mutation here doesn't leak across permutations.
            apply_env(perm);
            for fixture in fixtures {
                report.fixtures.push(run_one_fixture_embedded(fixture));
            }
        }
        Mode::Server => {
            // v7.38 day-6 work: spawn an in-process spg-server, dial it
            // via simple-query or extended protocol, drive the corpus
            // through the wire. The skeleton parses + dispatches but
            // returns `SkippedPending` so the CLI shape is correct now.
            report.status = PermStatus::SkippedPending(
                "server mode pending v7.38 day-6 ServerRunner work".into(),
            );
        }
    }

    report.duration_ms = started.elapsed().as_millis();
    report
}

fn apply_env(perm: &Permutation) {
    for (k, v) in &perm.env {
        // SAFETY: env mutation is process-global; parent runs each
        // permutation in a forked child so cross-permutation pollution
        // is structurally impossible. Within a child, fixtures share one
        // env (matches sqllogictest bin's existing semantics).
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var(k, v);
        }
    }
}

fn run_one_fixture_embedded(path: &Path) -> FixtureResult {
    let started = Instant::now();
    let mut result = FixtureResult {
        fixture: path.to_path_buf(),
        pass: 0,
        fail: 0,
        skip: 0,
        fail_snippets: Vec::new(),
        duration_ms: 0,
    };

    let records = match parser::parse_file(path) {
        Ok(rs) => rs,
        Err(e) => {
            result.fail = 1;
            result.fail_snippets.push(format!("parse: {e}"));
            result.duration_ms = started.elapsed().as_millis();
            return result;
        }
    };

    // Fresh Runner per fixture — matches the sqllogictest bin so the
    // baseline is byte-equal when `embedded` corpus_root + globs match.
    let mut runner = Runner::new();
    let outcome = runner.run(&records);
    result.pass = outcome.pass;
    result.fail = outcome.fail;
    result.skip = outcome.skip;
    for o in &outcome.per_record {
        if let sqllogictest::Outcome::Fail(msg) = o {
            if result.fail_snippets.len() < 3 {
                result.fail_snippets.push(short(msg));
            }
        }
    }
    result.duration_ms = started.elapsed().as_millis();
    result
}

fn short(s: &str) -> String {
    let one = s.replace('\n', " | ");
    if one.len() > 160 {
        format!("{}…", &one[..160])
    } else {
        one
    }
}
