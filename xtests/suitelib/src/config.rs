//! `xtests/suite.toml` — the manifest, parsed by hand.
//!
//! The file uses a deliberate TOML SUBSET: `[suite]`, repeated
//! `[[step]]`, `key = "string"` and `key = integer`. Anything else is
//! a loud error, not a guess — the manifest is a contract, and a
//! parser that shrugs at a typo'd section turns a missing step into a
//! silently thinner tier. Zero dependencies because this sits on the
//! precommit critical path (audit A13/D2).

#[derive(Debug, Default, Clone)]
pub struct SuiteMeta {
    pub current_pin_prefix: String,
    pub port_lo: u16,
    pub port_hi: u16,
    /// D20 — peak-RSS ceiling (MB) for suite-owned server processes.
    pub rss_ceiling_mb: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Step {
    pub tier: String,
    pub name: String,
    pub implementation: String,
    pub cmd: Option<String>,
    pub budget_s: Option<u64>,
    /// D23/S4.6 — adjacent steps sharing a group value run
    /// concurrently; the ledger still records them in file order.
    pub group: Option<String>,
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub meta: SuiteMeta,
    pub steps: Vec<Step>,
}

impl Manifest {
    /// Steps of one tier, in file order.
    #[must_use]
    pub fn tier(&self, tier: &str) -> Vec<&Step> {
        self.steps.iter().filter(|s| s.tier == tier).collect()
    }

    /// Parse the manifest subset.
    ///
    /// # Errors
    /// Any line that is not blank, a comment, one of the two known
    /// section headers, or a `key = value` inside a known section.
    pub fn parse(text: &str) -> Result<Self, String> {
        #[derive(PartialEq)]
        enum Sect {
            None,
            Suite,
            Step,
        }
        let mut out = Manifest::default();
        let mut sect = Sect::None;
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line {
                "[suite]" => {
                    sect = Sect::Suite;
                    continue;
                }
                "[[step]]" => {
                    sect = Sect::Step;
                    out.steps.push(Step::default());
                    continue;
                }
                _ => {}
            }
            if line.starts_with('[') {
                return Err(format!("suite.toml:{}: unknown section {line}", ln + 1));
            }
            let (key, val) = line
                .split_once('=')
                .ok_or_else(|| format!("suite.toml:{}: expected key = value", ln + 1))?;
            let (key, val) = (key.trim(), val.trim());
            let as_str = || -> Result<String, String> {
                val.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .map(str::to_string)
                    .ok_or_else(|| format!("suite.toml:{}: {key} wants a quoted string", ln + 1))
            };
            let as_int = || -> Result<u64, String> {
                val.parse()
                    .map_err(|_| format!("suite.toml:{}: {key} wants an integer", ln + 1))
            };
            match (&sect, key) {
                (Sect::Suite, "current_pin_prefix") => out.meta.current_pin_prefix = as_str()?,
                (Sect::Suite, "port_lo") => {
                    out.meta.port_lo = u16::try_from(as_int()?)
                        .map_err(|_| format!("suite.toml:{}: port_lo out of range", ln + 1))?;
                }
                (Sect::Suite, "port_hi") => {
                    out.meta.port_hi = u16::try_from(as_int()?)
                        .map_err(|_| format!("suite.toml:{}: port_hi out of range", ln + 1))?;
                }
                (Sect::Suite, "rss_ceiling_mb") => {
                    out.meta.rss_ceiling_mb = as_int()?;
                }
                (Sect::Step, k) => {
                    let step = out
                        .steps
                        .last_mut()
                        .ok_or_else(|| format!("suite.toml:{}: key before [[step]]", ln + 1))?;
                    match k {
                        "tier" => step.tier = as_str()?,
                        "name" => step.name = as_str()?,
                        "impl" => step.implementation = as_str()?,
                        "cmd" => step.cmd = Some(as_str()?),
                        "budget_s" => step.budget_s = Some(as_int()?),
                        "group" => step.group = Some(as_str()?),
                        other => {
                            return Err(format!("suite.toml:{}: unknown step key {other}", ln + 1));
                        }
                    }
                }
                (_, other) => {
                    return Err(format!(
                        "suite.toml:{}: key {other} outside a section",
                        ln + 1
                    ));
                }
            }
        }
        // Contract checks: every step fully named, external steps carry
        // a cmd, tiers restricted to the three known words.
        for s in &out.steps {
            if s.tier.is_empty() || s.name.is_empty() || s.implementation.is_empty() {
                return Err(format!("suite.toml: step {s:?} is missing tier/name/impl"));
            }
            if !matches!(s.tier.as_str(), "precommit" | "prerelease" | "full") {
                return Err(format!(
                    "suite.toml: unknown tier {} on step {}",
                    s.tier, s.name
                ));
            }
            if s.implementation == "external" && s.cmd.is_none() {
                return Err(format!("suite.toml: external step {} has no cmd", s.name));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_manifest() {
        let text = include_str!("../../suite.toml");
        let m = Manifest::parse(text).expect("the checked-in manifest must parse");
        assert_eq!(m.meta.current_pin_prefix, "pin_v738_");
        assert_eq!((m.meta.port_lo, m.meta.port_hi), (25460, 25479));
        for tier in ["precommit", "prerelease", "full"] {
            assert!(!m.tier(tier).is_empty(), "{tier} has steps");
        }
        // Every external step has a cmd (enforced), and precommit's
        // nominal budgets sum under the hard cap with headroom (A1).
        let pre_sum: u64 = m.tier("precommit").iter().filter_map(|s| s.budget_s).sum();
        assert!(pre_sum <= 120, "precommit nominal sum {pre_sum} > 120");
    }

    #[test]
    fn loud_errors_not_guesses() {
        for (bad, needle) in [
            ("[server]\n", "unknown section"),
            ("x = 1\n", "outside a section"),
            (
                "[[step]]\ntier = \"precommit\"\nname = \"x\"\nimpl = \"external\"\n",
                "no cmd",
            ),
            (
                "[[step]]\ntier = \"nightly\"\nname = \"x\"\nimpl = \"internal\"\n",
                "unknown tier",
            ),
            ("[suite]\nport_lo = \"low\"\n", "wants an integer"),
        ] {
            let e = Manifest::parse(bad).expect_err(bad);
            assert!(e.contains(needle), "{bad:?} -> {e}");
        }
    }
}

/// v7.38.19 — the recorded-delta register, checked in both directions.
///
/// A "recorded delta" is a comment saying *we know this differs from
/// PostgreSQL and here is why*. There were seventeen of them and no
/// list, so nobody could read them all, and not one had been
/// re-measured since the day it was written.
///
/// Re-measuring the nine in `crates/*/src` against a live PG 18.4 found
/// one that no longer reproduced, one stated backwards, one already
/// closed, two open — and one nobody had recorded at all, in the
/// direction that matters most: SPG accepting `1::xid8` where PG
/// rejects it.
///
/// So the register is not documentation, it is a gate. A marker with no
/// row is red; a row whose marker has gone is red. A delta that cannot
/// be forgotten is one somebody eventually closes.
#[cfg(test)]
mod recorded_delta_register {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf()
    }

    /// Every `RD-n` a source comment claims, from the crates' `src/`
    /// trees only: a test may quote a delta while discussing it.
    fn ids_in_source(root: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut stack = vec![root.join("crates")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    // `src` only — tests discuss deltas without owning them.
                    if p.file_name().is_some_and(|n| n == "tests" || n == "target") {
                        continue;
                    }
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let Ok(text) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    for line in text.lines() {
                        let t = line.trim_start();
                        if !t.starts_with("//") {
                            continue;
                        }
                        let mut rest = t;
                        while let Some(i) = rest.find("RD-") {
                            rest = &rest[i + 3..];
                            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
                            if !n.is_empty() {
                                out.insert(format!("RD-{n}"));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// v7.39.11 — the register lives under `tmp/`, which is internal
    /// and not in the published tree.
    ///
    /// It moved there when the repository stopped carrying development
    /// and process material, and this reader was not moved with it, so
    /// the row has been red on every full run since — which is to say
    /// on none of them, because the two releases in between skipped the
    /// gate. `prod_ready` had this exact problem with the internal docs
    /// and answered it the same way: a check whose SOURCE is absent
    /// skips rather than fails, because a clone that never had the file
    /// is not a tree that lost its register.
    const REGISTER: &str = "tmp/docs/RECORDED_DELTAS.md";

    fn ids_in_register(root: &Path) -> Option<BTreeSet<String>> {
        let text = std::fs::read_to_string(root.join(REGISTER)).ok()?;
        let mut out = BTreeSet::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("| RD-") {
            rest = &rest[i + 5..];
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !n.is_empty() {
                out.insert(format!("RD-{n}"));
            }
        }
        Some(out)
    }

    #[test]
    fn every_marker_has_a_row_and_every_row_has_a_marker() {
        let root = root();
        let src = ids_in_source(&root);
        assert!(!src.is_empty(), "no RD- markers found — the scan is broken");
        let Some(reg) = ids_in_register(&root) else {
            // Say so rather than passing quietly: a skip nobody can see
            // is the same as no gate.
            println!("{REGISTER} is not in this tree — register check skipped");
            return;
        };

        let unlisted: Vec<&String> = src.difference(&reg).collect();
        assert!(
            unlisted.is_empty(),
            "a source comment records these deltas and {REGISTER} \
             does not list them: {unlisted:?}. Measure both engines and add a row."
        );

        // The other direction. A row whose marker has gone means the
        // delta was closed and the register kept announcing it — which
        // is how the compatibility matrix came to carry twenty-one
        // crosses for code that worked.
        let orphaned: Vec<&String> = reg
            .difference(&src)
            .filter(|id| {
                // Rows under "corrected by measurement" and "not
                // previously recorded" describe deltas with no marker on
                // purpose; they carry their own ids and are listed here.
                !matches!(id.as_str(), "RD-7" | "RD-8" | "RD-9" | "RD-10" | "RD-11")
            })
            .collect();
        assert!(
            orphaned.is_empty(),
            "{REGISTER} lists these deltas and no source comment \
             records them: {orphaned:?}. If they were closed, delete the rows."
        );
    }
}

/// v7.38.19 — a test's scratch directory belongs under one root.
///
/// 161 files built a unique path under `std::env::temp_dir().join("spg-tests")` per run
/// and none removed it. On the machine this was found on `$TMPDIR` had
/// reached 61,708 entries and 30 GB — and it was not only disk:
/// `spg-server` swept that directory at every start, so one `readdir`
/// took 95 seconds and every server an e2e test spawned waited a minute
/// and a half before it could listen. The failures read exactly like a
/// busy machine, and were put down to one.
///
/// They now go under `$TMPDIR/spg-tests/`, which makes the mess
/// removable in one pass. This keeps it that way: a bare
/// `std::env::temp_dir().join("spg-tests")` in a test is red.
#[cfg(test)]
mod test_scratch_root {
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn no_test_writes_to_the_bare_temp_dir() {
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![root().join("crates"), root().join("xtests")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                // `src/` is the product; it may use the temp directory
                // as an operator's temp directory, which is what
                // `SPG_TEMP_DIR` documents. This is about TESTS.
                let is_test = p.components().any(|c| c.as_os_str() == "tests")
                    || p.file_name().is_some_and(|n| n == "config.rs");
                if !is_test {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                // v7.38.19 — the shared root may sit on the same line or
                // on the next, because rustfmt breaks a long chain:
                //
                //     let dir = std::env::temp_dir()
                //         .join("spg-tests")
                //         .join(format!(…));
                //
                // The first version compared only the rest of the same
                // line and called that correct code a violation. The
                // second version fixed the multi-line case and stopped
                // seeing the single-line one — caught by running BOTH
                // negative controls, which is the only reason this
                // paragraph is not describing a checker that passes
                // everything.
                //
                // So: join the call's line with the one after it and look
                // once. Two forms, one test.
                let lines: Vec<&str> = text.lines().collect();
                let needle = concat!("env::temp", "_dir()");
                for (i, line) in lines.iter().enumerate() {
                    let t = line.trim_start();
                    if t.starts_with("//") || !t.contains(needle) || t.contains("tmp_base") {
                        continue;
                    }
                    let next = lines.get(i + 1).map_or("", |l| l.trim());
                    let after = t.split_once(needle).map_or("", |(_, r)| r);
                    let joined = format!("{after} {next}");
                    if !joined.trim_start().starts_with(".join(\"spg-tests\")") {
                        offenders.push(format!("{}:{}", p.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these tests write scratch straight into $TMPDIR instead of \
             $TMPDIR/spg-tests — 161 files doing that reached 30 GB and made \
             every server start wait 95 seconds on one readdir:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// v7.38.19 — and the other way of writing the same mistake.
    ///
    /// The check above scans for `env::temp_dir()`. The suite's own
    /// `run_tmp_dir` did not call it — it built `/tmp/spg-suite-<runid>`
    /// by hand — so the suite's run directories were exactly what the
    /// gate was written to stop and the gate could not see them. Found by
    /// reading a process listing during a run, not by the check.
    ///
    /// A literal `/tmp/spg` outside the shared root is red for the same
    /// reason: one `rm -rf` should collect everything a run leaves.
    #[test]
    fn nothing_builds_a_scratch_path_out_of_a_bare_tmp_literal() {
        let root = root();
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![root.join("crates"), root.join("xtests")];
        let needle = concat!("\"/tmp", "/spg");
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    let t = line.trim_start();
                    if t.starts_with("//") || !t.contains(needle) {
                        continue;
                    }
                    if t.contains("/tmp/spg-tests") {
                        continue;
                    }
                    // A literal that never becomes a path on disk is not
                    // a leak. `parse_copy_intent("COPY t TO '/tmp/…'")`
                    // is a parser test asserting on a string, and the
                    // first version of this check called it a violation —
                    // the same over-broad reading that made the previous
                    // check red on four already-correct files.
                    //
                    // The signal is USE: a scratch path is built to be
                    // created, opened or handed to a process.
                    let creates = t.contains("PathBuf::from")
                        || t.contains("create_dir")
                        || t.contains("File::create")
                        || t.contains("write(")
                        || t.contains("remove_dir");
                    if !creates {
                        continue;
                    }
                    offenders.push(format!("{}:{}", p.display(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these build a scratch path from a bare /tmp literal instead of \
             /tmp/spg-tests, which puts them outside the one directory a \
             cleanup can collect:\n  {}",
            offenders.join("\n  ")
        );
    }
}

/// v7.39.12 — the unit tier does not spawn a harness that carries no
/// tests.
///
/// `cargo test --workspace --lib --bins` selected 83 harnesses across
/// this workspace and **64 of them carried zero tests**. Sixty-two of
/// those were binary targets, and 56 of the 62 belonged to
/// `spg-bench-competitor` — the side-by-side benchmark harness whose
/// `main`s time SPG against PostgreSQL and MySQL, and whose manifest
/// already declares `test = false` for them.
///
/// It declares it and the flag overrode it: an explicit target
/// selector wins over the manifest. Measured on `heavy`, which the
/// manifest marks `test = false` — `--tests` selects it 0 times and
/// `--lib --bins` selects it once; over nine benchmark bins, 1 against
/// 9. Excluding the crate says the same thing in the place that is
/// honoured: 84 harnesses became 27, and the test count did not move
/// (1,206 before, 1,206 after).
///
/// This row asserts the SHAPE, in counts rather than seconds, so it
/// reads the same on any machine: the tier's own `unit` command must
/// exclude the benchmark crate, and no other step may reintroduce
/// `--bins` over the whole workspace.
///
/// Why it is worth a row: each harness is a process spawn, and a spawn
/// is only free on an idle machine. The tier measured 10,777 s on the
/// testbed while another workload on the same box held `syspolicyd` at
/// 90% CPU; a probe binary that took 99 s to launch under that load
/// runs in under a second when the queue is empty. None of that was
/// the tier's work — but the spawns are the part this repository can
/// decide not to make.
#[cfg(test)]
mod unit_tier_spawns {
    use std::path::{Path, PathBuf};

    fn gate_sh() -> String {
        let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();
        std::fs::read_to_string(root.join("scripts/gate.sh")).expect("scripts/gate.sh")
    }

    #[test]
    fn every_workspace_wide_selection_excludes_the_benchmark_crate() {
        // Only WORKSPACE-wide selections: a `-p spg-server --tests`
        // leg names one crate and never reaches the benchmark crate,
        // and cargo rejects `--exclude` without `--workspace` anyway.
        // This row found that distinction itself, on the
        // shipped-collation leg.
        let sh = gate_sh();
        let lines: Vec<&str> = sh
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.starts_with('#')
                    && l.contains("cargo test")
                    && l.contains("--workspace")
                    && (l.contains("--bins") || l.contains("--tests"))
            })
            .collect();
        assert!(
            !lines.is_empty(),
            "no workspace-wide `cargo test … --bins/--tests` left in gate.sh — \
             if the tier stopped selecting them, delete this row and say why"
        );
        for l in &lines {
            assert!(
                l.contains("--exclude spg-bench-competitor"),
                "this line selects the benchmark crate's targets, which spawns \
                 harnesses carrying no tests (56 for --bins, 40 for --tests): {l}"
            );
        }
    }

    #[test]
    fn the_binaries_that_do_carry_tests_are_named_here() {
        // Six binary targets carry tests and must keep running:
        // spg-server (108), spgctl (23), the oracle (8), perm (3) and
        // dogfood (2) runners, and sqllogictest (1). None of them is in
        // the benchmark crate, which is why excluding it costs nothing.
        // If one of them ever moves INTO that crate, this row is the
        // place that says the exclude has to be revisited.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();
        let manifest = std::fs::read_to_string(root.join("xbench/competitor/Cargo.toml"))
            .expect("xbench/competitor/Cargo.toml");
        for name in ["spg-server", "spgctl", "spg-oracle-runner", "sqllogictest"] {
            assert!(
                !manifest.contains(&format!("name = \"{name}\"")),
                "{name} carries tests and has moved into the crate the unit                  step excludes"
            );
        }
    }
}
