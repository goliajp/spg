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

    fn ids_in_register(root: &Path) -> BTreeSet<String> {
        let text = std::fs::read_to_string(root.join("docs/RECORDED_DELTAS.md"))
            .expect("docs/RECORDED_DELTAS.md must exist");
        let mut out = BTreeSet::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("| RD-") {
            rest = &rest[i + 5..];
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !n.is_empty() {
                out.insert(format!("RD-{n}"));
            }
        }
        out
    }

    #[test]
    fn every_marker_has_a_row_and_every_row_has_a_marker() {
        let root = root();
        let src = ids_in_source(&root);
        let reg = ids_in_register(&root);
        assert!(!src.is_empty(), "no RD- markers found — the scan is broken");

        let unlisted: Vec<&String> = src.difference(&reg).collect();
        assert!(
            unlisted.is_empty(),
            "a source comment records these deltas and docs/RECORDED_DELTAS.md \
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
            "docs/RECORDED_DELTAS.md lists these deltas and no source comment \
             records them: {orphaned:?}. If they were closed, delete the rows."
        );
    }
}

/// v7.38.19 — a test's scratch directory belongs under one root.
///
/// 161 files built a unique path under `std::env::temp_dir()` per run
/// and none removed it. On the machine this was found on `$TMPDIR` had
/// reached 61,708 entries and 30 GB — and it was not only disk:
/// `spg-server` swept that directory at every start, so one `readdir`
/// took 95 seconds and every server an e2e test spawned waited a minute
/// and a half before it could listen. The failures read exactly like a
/// busy machine, and were put down to one.
///
/// They now go under `$TMPDIR/spg-tests/`, which makes the mess
/// removable in one pass. This keeps it that way: a bare
/// `std::env::temp_dir()` in a test is red.
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
                for (i, line) in text.lines().enumerate() {
                    let t = line.trim_start();
                    if t.starts_with("//") || t.starts_with("///") {
                        continue;
                    }
                    let needle = concat!("env::temp", "_dir()");
                    if let Some(rest) = t.split_once(needle).map(|(_, r)| r)
                        && !rest.trim_start().starts_with(".join(\"spg-tests\")")
                        && !t.contains("tmp_base")
                    {
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
}
