//! spg-perm-runner — v7.38 element B CLI.
//!
//! Hand-rolled arg parsing to keep the build cost off the gate.sh fast path
//! (no clap pull; the surface is tiny). Sub-commands:
//!
//!   list                       — print all permutations defined in TOML
//!   verify                     — parse TOML, validate, no execution
//!   run --permutation <NAME>   — fork-isolated single permutation run
//!   all [--fast | --full]      — run multiple permutations, child per perm
//!
//! Process model — the `all` command discovers permutations from TOML and
//! re-execs `current_exe` once per permutation with `run --permutation X`
//! and a child env extended by that permutation's `[permutation.env]`
//! table. Child writes `target/perm-runner/<perm>.json`; parent aggregates
//! to `target/perm-runner/report.json` and prints a three-column summary.
//!
//! Per-permutation fork-isolation is the v7.38 plan section 1.B requirement:
//! engine-level static state (plan cache, catalog) can't be safely reset
//! mid-process across permutations, and a panic in one permutation must
//! not poison the next.

#![allow(
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::uninlined_format_args,
    clippy::format_push_string
)]

mod permutation;
mod report;
mod runner;

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::permutation::PermFile;
use crate::runner::{PermStatus, discover_corpus, discover_sample, run_permutation};

const REPORT_DIR: &str = "target/perm-runner";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        return usage();
    }

    let cmd = args[1].as_str();
    let rest = &args[2..];

    let workspace_root = workspace_root();
    let perm_file_path = workspace_root.join("xtests/perm-runner/tests/permutations.toml");
    let perm_file = match PermFile::load(&perm_file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error loading {}: {e}", perm_file_path.display());
            return ExitCode::from(2);
        }
    };

    match cmd {
        "list" => cmd_list(&perm_file),
        "verify" => cmd_verify(&perm_file, &workspace_root),
        "run" => cmd_run(&perm_file, rest, &workspace_root),
        "all" => cmd_all(&perm_file, rest, &workspace_root),
        "-h" | "--help" => usage(),
        other => {
            eprintln!("unknown command `{other}`");
            usage()
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "spg-perm-runner — v7.38 element B permutation matrix runner

USAGE:
  spg-perm-runner <COMMAND>

COMMANDS:
  list                       List all permutations defined in tests/permutations.toml
  verify                     Parse TOML, validate, no execution
  run --permutation <NAME>   Run one permutation against the corpus
  all [--fast | --full]      Run the fast tier (3 core) or every non-full-only permutation

FLAGS for `run`:
  --sample                   Restrict to the fast_tier_sample subdir only

FLAGS for `all`:
  --fast                     Use fast tier permutations + fast_tier_sample corpus
  --full                     Include full_only permutations + complete corpus
"
    );
    ExitCode::from(2)
}

fn cmd_list(file: &PermFile) -> ExitCode {
    println!("{:<18} {:<9} {:<7} env", "name", "mode", "full");
    for p in &file.permutations {
        let mode_str = match p.mode {
            permutation::Mode::Embedded => "embedded",
            permutation::Mode::Server => "server",
        };
        let env_str: Vec<String> = p.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!(
            "{:<18} {:<9} {:<7} {}",
            p.name,
            mode_str,
            if p.full_only { "yes" } else { "no" },
            env_str.join(" ")
        );
    }
    ExitCode::SUCCESS
}

fn cmd_verify(file: &PermFile, workspace_root: &Path) -> ExitCode {
    // 1. Default block sanity.
    if file.permutations.is_empty() {
        eprintln!("verify: no permutations defined");
        return ExitCode::from(1);
    }

    // 2. fast_tier names resolve.
    for name in &file.default.fast_tier {
        if file.by_name(name).is_none() {
            eprintln!("verify: fast_tier references unknown permutation `{name}`");
            return ExitCode::from(1);
        }
    }

    // 3. corpus_root exists (warn but tolerate empty include subdirs).
    let root = workspace_root.join(&file.default.corpus_root);
    if !root.is_dir() {
        eprintln!("verify: corpus_root not a directory: {}", root.display());
        return ExitCode::from(1);
    }

    // 4. Permutation names unique.
    let mut seen = std::collections::HashSet::new();
    for p in &file.permutations {
        if !seen.insert(&p.name) {
            eprintln!("verify: duplicate permutation name `{}`", p.name);
            return ExitCode::from(1);
        }
    }

    println!(
        "verify: ok — {} permutations, fast_tier={:?}",
        file.permutations.len(),
        file.default.fast_tier
    );
    ExitCode::SUCCESS
}

fn cmd_run(file: &PermFile, rest: &[String], workspace_root: &Path) -> ExitCode {
    let mut perm_name: Option<String> = None;
    let mut sample_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--permutation" => {
                i += 1;
                if i >= rest.len() {
                    eprintln!("--permutation needs a value");
                    return ExitCode::from(2);
                }
                perm_name = Some(rest[i].clone());
            }
            "--sample" => sample_only = true,
            other => {
                eprintln!("unknown run flag `{other}`");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let name = match perm_name {
        Some(n) => n,
        None => {
            eprintln!("run: --permutation NAME is required");
            return ExitCode::from(2);
        }
    };
    let perm = match file.by_name(&name) {
        Some(p) => p.clone(),
        None => {
            eprintln!("run: unknown permutation `{name}`");
            return ExitCode::from(1);
        }
    };

    let fixtures = if sample_only {
        match discover_sample(workspace_root, file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("run: discover_sample: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        match discover_corpus(workspace_root, file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("run: discover_corpus: {e}");
                return ExitCode::from(1);
            }
        }
    };

    eprintln!(
        "[perm-runner] permutation={name} fixtures={} sample_only={sample_only}",
        fixtures.len()
    );
    let report = run_permutation(&perm, &fixtures);
    let out_path = workspace_root
        .join(REPORT_DIR)
        .join(format!("{}.json", perm.name));
    if let Err(e) = report::write_perm_report(&out_path, &report) {
        eprintln!("run: write report: {e}");
        return ExitCode::from(1);
    }
    let (p, f, s) = report.totals();
    let status = match &report.status {
        PermStatus::Ran => format!("pass={p} fail={f} skip={s}"),
        PermStatus::SkippedPending(msg) => format!("SKIPPED_PENDING: {msg}"),
    };
    println!(
        "[perm-runner] {} {} ({}ms) -> {}",
        perm.name,
        status,
        report.duration_ms,
        out_path.display()
    );

    if f > 0 {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn cmd_all(file: &PermFile, rest: &[String], workspace_root: &Path) -> ExitCode {
    let mut tier = Tier::Default;
    for arg in rest {
        match arg.as_str() {
            "--fast" => tier = Tier::Fast,
            "--full" => tier = Tier::Full,
            other => {
                eprintln!("unknown all flag `{other}`");
                return ExitCode::from(2);
            }
        }
    }

    let perm_names: Vec<String> = match tier {
        Tier::Fast => file.fast_tier_names().to_vec(),
        Tier::Default => file.non_full_only().map(|p| p.name.clone()).collect(),
        Tier::Full => file.permutations.iter().map(|p| p.name.clone()).collect(),
    };

    eprintln!("[perm-runner] tier={tier:?} permutations={:?}", perm_names);

    // Find ourselves so we can re-exec per permutation. Fork model:
    // parent runs only the dispatcher; each permutation gets its own
    // child process with its env extended by the permutation's [env]
    // table — engine-level statics are reset per process, and a panic
    // in one permutation can't poison another.
    let self_exe = match env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("current_exe: {e}");
            return ExitCode::from(2);
        }
    };

    let mut overall_fail = 0usize;
    let mut overall_pass = 0usize;
    let mut overall_skip = 0usize;
    let mut child_summaries: Vec<(String, i32, u128)> = Vec::new();

    let started = std::time::Instant::now();
    for name in &perm_names {
        let perm = match file.by_name(name) {
            Some(p) => p,
            None => {
                eprintln!("all: unknown permutation `{name}` — skipped");
                continue;
            }
        };
        eprintln!("[perm-runner] spawn child for `{name}`");
        let mut cmd = Command::new(&self_exe);
        cmd.arg("run").arg("--permutation").arg(name);
        if matches!(tier, Tier::Fast) {
            cmd.arg("--sample");
        }
        for (k, v) in &perm.env {
            cmd.env(k, v);
        }
        // Don't pipe stdout/stderr — let it stream so a hang is visible.
        let child_started = std::time::Instant::now();
        let status = match cmd.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("all: spawn `{name}`: {e}");
                overall_fail += 1;
                child_summaries.push((name.clone(), -1, child_started.elapsed().as_millis()));
                continue;
            }
        };
        let code = status.code().unwrap_or(-1);
        child_summaries.push((name.clone(), code, child_started.elapsed().as_millis()));
        if code != 0 {
            overall_fail += 1;
        }

        // Tally the per-perm report for the aggregate.
        let report_path = workspace_root.join(REPORT_DIR).join(format!("{name}.json"));
        if let Ok(s) = std::fs::read_to_string(&report_path) {
            // Very small structured peek — pass/fail/skip counts.
            let (p, f, k) = parse_totals(&s);
            overall_pass += p;
            overall_fail += f;
            overall_skip += k;
        }
    }
    let elapsed = started.elapsed();

    // Three-column-ish summary to stdout.
    println!(
        "\n=== perm-runner summary ({:.2}s, tier={:?}) ===",
        elapsed.as_secs_f64(),
        tier
    );
    println!("{:<18} {:>6} {:>8}", "permutation", "exit", "ms");
    for (name, code, ms) in &child_summaries {
        println!("{:<18} {:>6} {:>8}", name, code, ms);
    }
    println!("\noverall pass={overall_pass} fail={overall_fail} skip={overall_skip}");

    if overall_fail > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Debug, Clone, Copy)]
enum Tier {
    Default,
    Fast,
    Full,
}

fn parse_totals(json: &str) -> (usize, usize, usize) {
    // Hand-rolled: look for `"totals": { "pass": X, "fail": Y, "skip": Z }`.
    let find_after = |key: &str| -> Option<usize> {
        let needle = format!("\"{key}\":");
        let idx = json.find(&needle)? + needle.len();
        let rest = &json[idx..];
        let trimmed = rest.trim_start();
        let end = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        trimmed[..end].parse().ok()
    };
    let pass = find_after("pass").unwrap_or(0);
    let fail = find_after("fail").unwrap_or(0);
    let skip = find_after("skip").unwrap_or(0);
    (pass, fail, skip)
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR -> xtests/perm-runner; up two levels -> workspace.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(here)
}
