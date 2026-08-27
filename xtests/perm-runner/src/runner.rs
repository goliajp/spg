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
            // r1056 (S3.5) — the ServerRunner: a REAL spg-server per
            // fixture (same isolation as the fresh embedded Runner),
            // driven over pgwire. `server_simple` uses the simple-query
            // protocol; `server_extended` sends every record through
            // Parse/Bind/Describe/Execute/Sync — the road actual
            // drivers ride, which is where three sentori walls lived.
            apply_env(perm);
            let extended = perm.name.contains("extended");
            let bin = std::path::Path::new("target/release/spg-server");
            if !bin.exists() {
                report.status = PermStatus::SkippedPending(
                    "target/release/spg-server not built — run `cargo build --release -p spg-server` first".into(),
                );
            } else {
                for fixture in fixtures {
                    report
                        .fixtures
                        .push(run_one_fixture_server(fixture, extended, bin));
                }
            }
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

fn run_one_fixture_server(path: &Path, extended: bool, bin: &Path) -> FixtureResult {
    use sqllogictest::parser::{ExpectedQuery, Record};
    let started = Instant::now();
    let mut result = FixtureResult {
        fixture: path.to_path_buf(),
        pass: 0,
        fail: 0,
        skip: 0,
        fail_snippets: Vec::new(),
        duration_ms: 0,
    };
    let mut fail = |r: &mut FixtureResult, msg: String| {
        r.fail += 1;
        if r.fail_snippets.len() < 3 {
            r.fail_snippets.push(short(&msg));
        }
    };
    let records = match parser::parse_file(path) {
        Ok(rs) => rs,
        Err(e) => {
            fail(&mut result, format!("parse: {e}"));
            result.duration_ms = started.elapsed().as_millis();
            return result;
        }
    };
    // Fresh server per fixture — the wire twin of the fresh Runner.
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("fixture");
    let tmp = suitelib::proclib::run_tmp_dir(&format!("perm-{stem}"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = suitelib::proclib::Roster::new();
    // Same deterministic instant as the embedded runner's
    // `fixed_test_clock` (2025-06-15T12:00:00Z) — one corpus, one
    // clock, whichever road executes it. Rides the r1058 GUC.
    let clock_env = [("SPG_TEST_FIXED_CLOCK_MICROS", "1749988800000000")];
    let port = match roster.spawn_server_env(
        stem,
        bin,
        &tmp,
        std::time::Duration::from_secs(20),
        "127.0.0.1",
        &clock_env,
    ) {
        Ok(p) => p,
        Err(e) => {
            fail(&mut result, format!("server spawn: {e}"));
            result.duration_ms = started.elapsed().as_millis();
            return result;
        }
    };
    let mut conn = match suitelib::wireclient::Conn::connect(port, "perm", "perm") {
        Ok(c) => c,
        Err(e) => {
            fail(&mut result, format!("connect: {e}"));
            result.duration_ms = started.elapsed().as_millis();
            return result;
        }
    };
    let mut run = |c: &mut suitelib::wireclient::Conn,
                   sql: &str|
     -> Result<suitelib::wireclient::QueryResult, String> {
        if extended {
            c.extended_query(sql)
        } else {
            c.simple_query(sql)
        }
    };
    for rec in &records {
        match rec {
            Record::Halt => break,
            // v7.38.17 — the dialect is a SESSION property, not a
            // protocol one. This runner drives a PostgreSQL wire
            // connection, and a PostgreSQL connection can still ask for
            // MySQL semantics: `SET sql_mode` is exactly the switch a
            // mysqldump preamble uses, and `session.rs:277` honours it
            // whichever wire it arrives on.
            //
            // This used to skip the rest of the file, which was a small
            // instance of the disease this whole change is about — a
            // skip that reads as coverage. Running it here is the
            // "MySQL semantics over a wire" axis, and the defects worth
            // catching there are engine-level: the packet layer has its
            // own eleven tests.
            Record::Dialect(mysql) => {
                let sql = if *mysql {
                    "SET sql_mode = 'STRICT_TRANS_TABLES'"
                } else {
                    // PostgreSQL's own switch for the same session
                    // property. `sql_mode = 'NO_BACKSLASH_ESCAPES'`
                    // would also land here, but it announces a MySQL
                    // client and carries strictness with it; a file
                    // asking for PostgreSQL should not have to.
                    "SET standard_conforming_strings = on"
                };
                match run(&mut conn, sql) {
                    Ok(r) if r.error.is_none() => result.pass += 1,
                    Ok(r) => fail(
                        &mut result,
                        format!("dialect switch rejected: {:?}", r.error),
                    ),
                    Err(e) => fail(&mut result, format!("wire: {e}")),
                }
            }
            Record::Statement {
                directive,
                sql,
                expect_error,
            } => {
                if directive.skip {
                    result.skip += 1;
                    continue;
                }
                match run(&mut conn, sql) {
                    Err(e) => fail(&mut result, format!("wire: {e}")),
                    Ok(r) => match (r.error, expect_error) {
                        (None, false) | (Some(_), true) => result.pass += 1,
                        (Some(e), false) => fail(&mut result, format!("{sql}: {e}")),
                        (None, true) => {
                            fail(&mut result, format!("expected error, got ok: {sql}"));
                        }
                    },
                }
            }
            Record::Query {
                directive,
                sql,
                type_string,
                sort,
                expected,
            } => {
                if directive.skip {
                    result.skip += 1;
                    continue;
                }
                let ExpectedQuery::Values(expected) = expected else {
                    result.skip += 1;
                    continue;
                };
                match run(&mut conn, sql) {
                    Err(e) => fail(&mut result, format!("wire: {e}")),
                    Ok(r) => {
                        if let Some(e) = r.error {
                            fail(&mut result, format!("{sql}: {e}"));
                            continue;
                        }
                        let mut actual = render_wire_cells(&r.rows, type_string);
                        sqllogictest::record::apply_sort(&mut actual, type_string, *sort);
                        if actual == *expected {
                            result.pass += 1;
                        } else {
                            fail(
                                &mut result,
                                format!(
                                    "row mismatch {sql}: expected {expected:?} actual {actual:?}"
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    result.duration_ms = started.elapsed().as_millis();
    result
}

/// Wire text → the embedded renderer's conventions, cell by cell, so
/// one corpus judges both roads: `B` prints 0/1, `R` prints three
/// decimals, an empty `T` prints `(empty)`. NULL already arrives as
/// the literal `NULL` from the wire client.
fn render_wire_cells(rows: &[Vec<String>], type_string: &str) -> Vec<String> {
    let types: Vec<char> = type_string.chars().collect();
    let mut out = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let ty = *types.get(i).unwrap_or(&'T');
            out.push(match ty {
                _ if cell == "NULL" => cell.clone(),
                'B' => match cell.as_str() {
                    "t" | "true" | "1" => "1".to_string(),
                    "f" | "false" | "0" => "0".to_string(),
                    other => other.to_string(),
                },
                // R/I pass through: the wire's float8out shortest form
                // is the same text `render_cell`'s `format_real` emits
                // (first full-corpus run proved reformatting wrong:
                // "2.5000000000000000" is NUMERIC text, not a float).
                'T' if cell.is_empty() => "(empty)".to_string(),
                _ => cell.clone(),
            });
        }
    }
    out
}

fn short(s: &str) -> String {
    let one = s.replace('\n', " | ");
    if one.chars().count() > 160 {
        // Count characters, not bytes. `&one[..160]` panics the moment
        // byte 160 lands inside a multi-byte character, and this string is
        // only ever built to report a failure -- the panic would replace
        // the failure being reported with one of its own. The same defect
        // was found the same day in `xtests/suitelib/src/steps.rs` and
        // `xtests/dogfood_replay/src/bench.rs`.
        format!("{}…", one.chars().take(160).collect::<String>())
    } else {
        one
    }
}

#[cfg(test)]
mod short_tests {
    use super::short;

    /// Same defect as `xtests/sqllogictest/src/main.rs` and
    /// `xtests/dogfood_replay/src/bench.rs`: truncating a failure
    /// reason by byte offset panics on a multi-byte boundary, which
    /// replaces the failure being reported with one of its own.
    #[test]
    fn short_does_not_panic_on_a_multibyte_boundary() {
        // 158 ASCII characters then a 3-byte one. The string is 161
        // bytes, so the old `len() > 160` test truncated it, and byte 160
        // lands INSIDE that last character -- which is exactly where
        // `&s[..160]` panicked. Counting characters, 159 is short enough
        // to keep whole.
        let s = format!("{}\u{4e2d}", "x".repeat(158));
        assert_eq!(s.len(), 161, "158 bytes plus a 3-byte character");
        assert_eq!(s.chars().count(), 159);
        assert_eq!(short(&s), s, "159 characters comes back whole");

        // One that really is too long, built so the cut itself lands
        // INSIDE a character: 158 ASCII bytes then multi-byte ones, so
        // byte 160 is the last third of the first of them. A byte slice
        // at [..160] panics here even when the length test counts
        // characters -- both halves of the fix are needed, and an
        // earlier version of this test proved only the first.
        let long = format!("{}{}", "x".repeat(158), "\u{4e2d}".repeat(10));
        assert!(
            !long.is_char_boundary(160),
            "the cut must land inside a character"
        );
        let cut = short(&long);
        assert!(
            cut.ends_with('\u{2026}'),
            "expected an ellipsis, got {cut:?}"
        );
        assert_eq!(cut.chars().count(), 161, "160 characters plus the ellipsis");
    }
}
