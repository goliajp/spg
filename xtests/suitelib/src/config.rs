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
