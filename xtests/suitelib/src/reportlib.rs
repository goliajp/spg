//! Timing ledger and run reports.
//!
//! Every tier run produces `target/suite/report-<tier>-<runid>.json`
//! plus a `latest-<tier>.json` copy (D3). Diffs between runs compare
//! STRUCTURE and STATUS only; durations are information, flagged when
//! outside a ±20% band (audit A14) — a suite whose own performance
//! drifts should say so without crying wolf on scheduler noise.
//!
//! JSON is written by hand: the shape is flat and fixed, and a serde
//! dependency on the precommit critical path is compile time spent on
//! nothing (audit A13).

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug)]
pub struct StepRecord {
    pub name: String,
    pub status: StepStatus,
    pub duration: Duration,
    pub budget: Option<Duration>,
}

pub struct Ledger {
    pub tier: String,
    pub runid: String,
    started: Instant,
    steps: Vec<StepRecord>,
}

/// `<UTCyyyymmddHHMMSS>-<gitsha7>` (D3). The caller supplies both —
/// this crate does not read the clock or the repo on its own, so a
/// run-id is reproducible in tests.
#[must_use]
pub fn runid(utc_stamp: &str, gitsha7: &str) -> String {
    format!("{utc_stamp}-{gitsha7}")
}

impl Ledger {
    #[must_use]
    pub fn new(tier: &str, runid: &str) -> Self {
        Self {
            tier: tier.to_string(),
            runid: runid.to_string(),
            started: Instant::now(),
            steps: Vec::new(),
        }
    }

    /// Time one step. The closure's Err is the step's failure text;
    /// the ledger records either way and hands the Err back up.
    ///
    /// # Errors
    /// Whatever the step itself returned; recording never fails.
    pub fn step<T>(
        &mut self,
        name: &str,
        budget: Option<Duration>,
        f: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let t0 = Instant::now();
        let out = f();
        self.steps.push(StepRecord {
            name: name.to_string(),
            status: if out.is_ok() {
                StepStatus::Pass
            } else {
                StepStatus::Fail
            },
            duration: t0.elapsed(),
            budget,
        });
        out
    }

    /// S4.6 — record a step that was executed OUT OF BAND (a parallel
    /// group runs steps on threads and records them here in FILE
    /// order, so the ledger stays deterministic however the threads
    /// finished).
    pub fn record_result(
        &mut self,
        name: &str,
        budget: Option<Duration>,
        duration: Duration,
        ok: bool,
    ) {
        self.steps.push(StepRecord {
            name: name.to_string(),
            status: if ok {
                StepStatus::Pass
            } else {
                StepStatus::Fail
            },
            duration,
            budget,
        });
    }

    pub fn record_skip(&mut self, name: &str) {
        self.steps.push(StepRecord {
            name: name.to_string(),
            status: StepStatus::Skipped,
            duration: Duration::ZERO,
            budget: None,
        });
    }

    /// Steps whose duration exceeded their budget — the tier's own
    /// perf gate (TIERS: 预算是闸不是愿望).
    #[must_use]
    pub fn over_budget(&self) -> Vec<&StepRecord> {
        self.steps
            .iter()
            .filter(|s| s.budget.is_some_and(|b| s.duration > b))
            .collect()
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let _ = writeln!(out, "  \"tier\": \"{}\",", self.tier);
        let _ = writeln!(out, "  \"runid\": \"{}\",", self.runid);
        let _ = writeln!(
            out,
            "  \"total_ms\": {},",
            self.started.elapsed().as_millis()
        );
        out.push_str("  \"steps\": [\n");
        for (i, s) in self.steps.iter().enumerate() {
            let status = match s.status {
                StepStatus::Pass => "pass",
                StepStatus::Fail => "fail",
                StepStatus::Skipped => "skipped",
            };
            let _ = write!(
                out,
                "    {{\"name\": \"{}\", \"status\": \"{}\", \"ms\": {}",
                s.name,
                status,
                s.duration.as_millis()
            );
            if let Some(b) = s.budget {
                let _ = write!(out, ", \"budget_ms\": {}", b.as_millis());
            }
            out.push('}');
            out.push_str(if i + 1 < self.steps.len() {
                ",\n"
            } else {
                "\n"
            });
        }
        out.push_str("  ]\n}\n");
        out
    }

    /// Write `report-<tier>-<runid>.json` and the `latest-<tier>.json`
    /// copy under `target/suite/`.
    ///
    /// # Errors
    /// Filesystem only.
    pub fn write(&self, target_dir: &std::path::Path) -> Result<PathBuf, String> {
        let dir = target_dir.join("suite");
        std::fs::create_dir_all(&dir).map_err(|e| format!("reportlib: mkdir: {e}"))?;
        let json = self.to_json();
        let path = dir.join(format!("report-{}-{}.json", self.tier, self.runid));
        std::fs::write(&path, &json).map_err(|e| format!("reportlib: write: {e}"))?;
        std::fs::write(dir.join(format!("latest-{}.json", self.tier)), &json)
            .map_err(|e| format!("reportlib: write latest: {e}"))?;
        Ok(path)
    }
}

/// Structure/status diff between two reports' step lists, parsed from
/// the JSON this module writes. Duration drift beyond ±20% is a
/// WARNING line, never a difference (A14).
#[must_use]
pub fn diff_reports(old: &str, new: &str) -> Vec<String> {
    let parse = |s: &str| -> Vec<(String, String, i64)> {
        s.lines()
            .filter(|l| l.trim_start().starts_with("{\"name\""))
            .filter_map(|l| {
                let name = l
                    .split("\"name\": \"")
                    .nth(1)?
                    .split('"')
                    .next()?
                    .to_string();
                let status = l
                    .split("\"status\": \"")
                    .nth(1)?
                    .split('"')
                    .next()?
                    .to_string();
                let ms = l
                    .split("\"ms\": ")
                    .nth(1)?
                    .split(['}', ','])
                    .next()?
                    .trim()
                    .parse()
                    .ok()?;
                Some((name, status, ms))
            })
            .collect()
    };
    let (a, b) = (parse(old), parse(new));
    let mut out = Vec::new();
    if a.iter().map(|x| &x.0).ne(b.iter().map(|x| &x.0)) {
        out.push("step list changed".to_string());
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if x.0 == y.0 && x.1 != y.1 {
            out.push(format!("{}: {} -> {}", x.0, x.1, y.1));
        }
        if x.0 == y.0 && x.2 > 0 && (y.2 as f64) > (x.2 as f64) * 1.2 {
            out.push(format!(
                "WARN {}: {} ms -> {} ms (+{}%)",
                x.0,
                x.2,
                y.2,
                (y.2 - x.2) * 100 / x.2
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_round_trips_and_diffs_clean_against_itself() {
        let mut l = Ledger::new("precommit", &runid("20260817120000", "abc1234"));
        l.step("fmt", Some(Duration::from_secs(5)), || Ok(()))
            .unwrap();
        l.record_skip("perf");
        let json = l.to_json();
        assert!(json.contains("\"tier\": \"precommit\""), "{json}");
        assert!(json.contains("\"fmt\""), "{json}");
        assert!(diff_reports(&json, &json).is_empty());
    }

    #[test]
    fn over_budget_and_status_flips_are_visible() {
        let mut l = Ledger::new("t", "r");
        let _ = l.step("slow", Some(Duration::ZERO), || {
            std::thread::sleep(Duration::from_millis(2));
            Ok(())
        });
        assert_eq!(l.over_budget().len(), 1);
        let ok = l.to_json();
        let mut l2 = Ledger::new("t", "r");
        let _ = l2.step("slow", Some(Duration::ZERO), || Err::<(), _>("boom".into()));
        let bad = l2.to_json();
        let d = diff_reports(&ok, &bad);
        assert!(d.iter().any(|x| x.contains("pass -> fail")), "{d:?}");
    }
}
