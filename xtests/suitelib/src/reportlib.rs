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
    /// v7.39.13 — how busy the machine was when this step finished.
    ///
    /// The run-level pair could not see the middle. A 103-minute
    /// prerelease reported 1.23 at the start and 3.67 at the end while
    /// one step inside it read 3,911 s against 201 s for the same
    /// command by hand — and nothing in the report could say whether
    /// the machine was quiet for that hour, because the only two
    /// samples sat either side of it.
    pub load_end: f64,
}

pub struct Ledger {
    pub tier: String,
    pub runid: String,
    /// v7.38.22 — the budget band this run was judged under.
    ///
    /// Recorded because a step's duration is only comparable to its own
    /// history at the SAME band: `unit-affected` has honestly taken 11 ms
    /// and 1,989,963 ms in this repository, and the difference is how many
    /// crates the commit touched.
    pub band: u64,
    /// v7.39.12 — how busy the machine was, at the start of the run and
    /// at the end.
    ///
    /// A step's duration is not a property of the step alone, and a
    /// report that does not say what else the machine was doing invites
    /// the reader to attribute the difference to the code. It did: this
    /// tier was read at 10,777 s and taken apart on the assumption that
    /// the time was its own work, when another workload on the same box
    /// had `syspolicyd` pinned at 90% CPU and a probe binary that
    /// launches in under a second was taking 99 s to start.
    ///
    /// One-minute load average, which is the cheapest thing that would
    /// have told that story. It is recorded, never judged — the numbers
    /// a busy machine produces are not wrong, they are about something
    /// else, and the reader is the one who has to know which.
    pub load_start: f64,
    pub load_end: Option<f64>,
    started: Instant,
    steps: Vec<StepRecord>,
}

/// The one-minute load average, or `-1.0` where the platform will not
/// say. Deliberately not an `Option` in the JSON: a missing field reads
/// as "nobody looked", and this is "the machine would not answer".
#[must_use]
pub fn load_avg_1m() -> f64 {
    let mut avg = [0f64; 3];
    // SAFETY: `getloadavg` writes at most `nelem` doubles into the
    // buffer and the buffer holds three.
    let n = unsafe { getloadavg(avg.as_mut_ptr(), 3) };
    if n >= 1 { avg[0] } else { -1.0 }
}

unsafe extern "C" {
    fn getloadavg(loadavg: *mut f64, nelem: i32) -> i32;
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
            band: 1,
            load_start: load_avg_1m(),
            load_end: None,
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
            load_end: load_avg_1m(),
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
            load_end: load_avg_1m(),
        });
    }

    pub fn record_skip(&mut self, name: &str) {
        self.steps.push(StepRecord {
            name: name.to_string(),
            status: StepStatus::Skipped,
            duration: Duration::ZERO,
            budget: None,
            load_end: load_avg_1m(),
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
        let _ = writeln!(out, "  \"band\": {},", self.band);
        let _ = writeln!(out, "  \"load_start\": {:.2},", self.load_start);
        let _ = writeln!(out, "  \"load_end\": {:.2},", self.load_end.unwrap_or(-1.0));
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
            let _ = write!(out, ", \"load_end\": {:.2}", s.load_end);
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
    pub fn write(&mut self, target_dir: &std::path::Path) -> Result<PathBuf, String> {
        self.load_end = Some(load_avg_1m());
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

/// v7.38.22 — what this step has taken on this host lately, at the same
/// band.
///
/// An over-budget verdict says a number was exceeded; it does not say
/// whether the step got slower or the machine did. Three sensors were
/// tried for that and all three were refuted by measurement (see the
/// v7.38.22 plan, item D3), so the run prints the evidence instead of
/// guessing: the same step, same band, most recent first.
///
/// Reports whose band is absent are from before this field existed and
/// are skipped rather than assumed to match.
#[must_use]
pub fn recent_step_ms(
    target_dir: &std::path::Path,
    tier: &str,
    step: &str,
    band: u64,
    n: usize,
) -> Vec<u64> {
    let dir = target_dir.join("suite");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = format!("report-{tier}-");
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with(&prefix) && f.ends_with(".json"))
        })
        .filter_map(|p| p.metadata().ok()?.modified().ok().map(|m| (m, p)))
        .collect();
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    let mut out = Vec::new();
    for (_, p) in files {
        if out.len() >= n {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        if field_u64(&text, "\"band\":") != Some(band) {
            continue;
        }
        // The step objects are one per line; find this step's `ms`.
        if let Some(line) = text
            .lines()
            .find(|l| l.contains(&format!("\"name\": \"{step}\"")))
            && let Some(ms) = field_u64(line, "\"ms\":")
        {
            out.push(ms);
        }
    }
    out
}

/// The first integer after `key` in `text`, if any.
fn field_u64(text: &str, key: &str) -> Option<u64> {
    let after = text.split(key).nth(1)?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod history_tests {
    use super::{Ledger, StepStatus, recent_step_ms};
    use std::time::Duration;

    fn write_run(dir: &std::path::Path, runid: &str, band: u64, unit_ms: u64) {
        let mut l = Ledger::new("precommit", runid);
        l.band = band;
        l.record_result(
            "unit-affected",
            Some(Duration::from_secs(35)),
            Duration::from_millis(unit_ms),
            true,
        );
        l.write(dir).expect("write");
        // Reports are ordered by mtime; make each one strictly newer.
        std::thread::sleep(Duration::from_millis(12));
    }

    #[test]
    fn reads_the_same_step_at_the_same_band_newest_first() {
        let dir = std::env::temp_dir().join(format!("spg-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_run(&dir, "a-1111111", 1, 900);
        write_run(&dir, "b-2222222", 8, 500_000);
        write_run(&dir, "c-3333333", 1, 34_644);
        let got = recent_step_ms(&dir, "precommit", "unit-affected", 1, 6);
        assert_eq!(
            got,
            vec![34_644, 900],
            "band 8 must not be mixed in, newest first"
        );
        let got8 = recent_step_ms(&dir, "precommit", "unit-affected", 8, 6);
        assert_eq!(got8, vec![500_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_step_that_never_ran_reads_empty_rather_than_zero() {
        let dir = std::env::temp_dir().join(format!("spg-hist-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_run(&dir, "a-1111111", 1, 900);
        assert!(recent_step_ms(&dir, "precommit", "no-such-step", 1, 6).is_empty());
        // And a band nobody has run at: empty, not the nearest one.
        assert!(recent_step_ms(&dir, "precommit", "unit-affected", 5, 6).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_is_honoured() {
        let dir = std::env::temp_dir().join(format!("spg-hist-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..5 {
            write_run(&dir, &format!("r{i}-1111111"), 1, 100 + i);
        }
        assert_eq!(
            recent_step_ms(&dir, "precommit", "unit-affected", 1, 2).len(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ledger_records_the_band_it_was_judged_under() {
        let mut l = Ledger::new("precommit", "z-9999999");
        l.band = 7;
        l.record_result("fmt", None, Duration::from_millis(1), true);
        assert!(l.to_json().contains("\"band\": 7"), "{}", l.to_json());
        assert!(matches!(l.over_budget().len(), 0), "no budget, no verdict");
        let _ = StepStatus::Pass;
    }
}
