#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::used_underscore_binding,
    clippy::nonminimal_bool,
    clippy::if_same_then_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::wildcard_imports,
    clippy::format_push_string,
    clippy::format_in_format_args,
    clippy::uninlined_format_args,
    clippy::redundant_closure_for_method_calls,
    clippy::collapsible_if,
    clippy::unnecessary_sort_by,
    clippy::map_unwrap_or
)]

//! sqllogictest CLI — walks `corpus/<group>/*.test`, runs each through a
//! fresh `spg_engine::Engine`, writes a `report.json` + `report.md`.
//!
//! Usage:
//! ```sh
//! cargo run -p sqllogictest --release
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sqllogictest::{Outcome, Runner, parser};

fn main() -> ExitCode {
    let workspace_root = workspace_root();
    // r1051 (7.38 S0.10/S2.1) — `--list <file>`: run ONLY the corpus
    // files named in the list (one relative path per line, `#` for
    // comments). The precommit tier's slt-smoke step runs this way.
    // A list run writes NO report.json/report.md: those two tracked
    // files are the FULL run's artifact, and a subset overwriting them
    // would masquerade as full coverage.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // r1052 (S2.2) — `--record <file…> [--oracle <PG_URI>]`: rewrite the
    // expected blocks of the NAMED files from actual output. Explicit
    // files only (design D7): a recorder that can sweep a directory
    // turns bugs into "known differences" wholesale, which is exactly
    // what the r1020 baseline did.
    // r1064 (S4.5) — `--docs [files…]`: execute every ```sql fence in
    // the public docs on the embedded engine (design D17).
    if let Some(i) = args.iter().position(|a| a == "--docs") {
        let files: Vec<String> = args[i + 1..].to_vec();
        let code = sqllogictest::docs::run(&files);
        return if code == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if let Some(i) = args.iter().position(|a| a == "--record") {
        let oracle_pos = args.iter().position(|a| a == "--oracle");
        let oracle: Option<String> = oracle_pos.and_then(|p| args.get(p + 1).cloned());
        let files: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(j, a)| {
                *j != i
                    && Some(*j) != oracle_pos
                    && Some(*j) != oracle_pos.map(|p| p + 1)
                    && !a.starts_with("--")
            })
            .map(|(_, a)| a)
            .collect();
        if files.is_empty() {
            eprintln!(
                "sqllogictest: --record refuses to run without explicit file paths \
                 (no directory sweeps — record what you mean to record)"
            );
            return ExitCode::from(2);
        }
        let mut failed = false;
        for f in files {
            let path = workspace_root.join(f);
            match sqllogictest::record::record_file(&path, oracle.as_deref()) {
                Ok((_, summary)) => println!("recorded {f}: {summary}"),
                Err(e) => {
                    eprintln!("record {f}: {e}");
                    failed = true;
                }
            }
        }
        return if failed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        };
    }
    if let Some(i) = args.iter().position(|a| a == "--list") {
        let Some(list_rel) = args.get(i + 1) else {
            eprintln!("sqllogictest: --list needs a file path");
            return ExitCode::from(2);
        };
        return run_list(&workspace_root, list_rel);
    }
    let corpus = workspace_root.join("xtests/sqllogictest/corpus");
    let report_json = workspace_root.join("xtests/sqllogictest/report.json");
    let report_md = workspace_root.join("xtests/sqllogictest/report.md");

    if !corpus.is_dir() {
        eprintln!("sqllogictest: corpus dir not found: {}", corpus.display());
        return ExitCode::from(1);
    }

    let mut groups: Vec<GroupReport> = Vec::new();
    let mut diffs: Vec<String> = Vec::new();
    for entry in fs::read_dir(&corpus).expect("read corpus") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let group_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        // If a top-level group dir has no `.test` files of its own but
        // does contain subdirectories, treat each subdirectory as its
        // own group named `<parent>/<sub>`. Lets `spg_baseline/` hold
        // a 14-directory taxonomy without flattening all files into one
        // big group.
        let has_own_tests = fs::read_dir(&path)
            .ok()
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("test"))
            })
            .unwrap_or(false);
        let subdirs: Vec<PathBuf> = fs::read_dir(&path)
            .ok()
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        if !has_own_tests && !subdirs.is_empty() {
            let mut sd = subdirs;
            sd.sort();
            for sub in sd {
                let sub_name = sub
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                let composite = format!("{group_name}/{sub_name}");
                groups.push(run_group(&composite, &sub, &mut diffs));
            }
        } else {
            groups.push(run_group(&group_name, &path, &mut diffs));
        }
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));

    write_json(&report_json, &groups).expect("write report.json");
    write_md(&report_md, &groups).expect("write report.md");

    println!("\n=== SPG conformance baseline ===");
    for g in &groups {
        let total = g.pass + g.fail + g.skip;
        let pct = if total == 0 {
            0.0
        } else {
            f64::from(u32::try_from(g.pass).unwrap_or(0))
                / f64::from(u32::try_from(total.max(1)).unwrap_or(1))
                * 100.0
        };
        println!(
            "{:<14} pass={:>3} fail={:>3} skip={:>3} ({:.1}%)",
            g.name, g.pass, g.fail, g.skip, pct
        );
    }
    println!("\nreport.json -> {}", report_json.display());
    println!("report.md   -> {}", report_md.display());

    // v7.39 (round 664) — this used to return SUCCESS unconditionally, so a
    // failing conformance test left no trace an automated caller could see.
    // `scripts/gate.sh` runs this under `set -euo pipefail` and trusted the
    // status; a stale assertion added in r661 therefore rode through THREE
    // green gate runs. The per-corpus lines above did print `fail=1`, but
    // only the last corpus is visible to anything that tails the output.
    //
    // So: one total line that cannot scroll away, every failing file named,
    // and a non-zero status.
    let pass: usize = groups.iter().map(|g| g.pass).sum();
    let fail: usize = groups.iter().map(|g| g.fail).sum();
    let skip: usize = groups.iter().map(|g| g.skip).sum();
    write_diffs(&workspace_root, &diffs);
    println!("\nTOTAL          pass={pass} fail={fail} skip={skip}");

    if fail == 0 {
        return ExitCode::SUCCESS;
    }
    println!("\nFAILING FILES:");
    for g in &groups {
        for f in g.files.iter().filter(|f| f.fail > 0) {
            println!("  {}/{} — {} failing", g.name, f.file, f.fail);
            for r in &f.fail_reasons {
                println!("      {r}");
            }
        }
    }
    ExitCode::FAILURE
}

#[derive(Debug, Clone)]
struct GroupReport {
    name: String,
    pass: usize,
    fail: usize,
    skip: usize,
    files: Vec<FileReport>,
}

#[derive(Debug, Clone)]
struct FileReport {
    file: String,
    pass: usize,
    fail: usize,
    skip: usize,
    /// First few failures' short text. We don't dump every failure into the
    /// report — the top patterns are enough to drive v1.2 hot planning.
    fail_reasons: Vec<String>,
}

/// r1051 — run the files a list names, print the un-scrollable TOTAL
/// line, return the same exit discipline as the full run (round 664:
/// a runner that fails politely is a runner nobody hears).
fn run_list(workspace_root: &Path, list_rel: &str) -> ExitCode {
    let list_path = workspace_root.join(list_rel);
    let text = match fs::read_to_string(&list_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("sqllogictest: {}: {e}", list_path.display());
            return ExitCode::from(2);
        }
    };
    let mut reports: Vec<FileReport> = Vec::new();
    let mut diffs: Vec<String> = Vec::new();
    let mut missing = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = workspace_root.join(line);
        if !path.is_file() {
            eprintln!("sqllogictest: listed file missing: {line}");
            missing += 1;
            continue;
        }
        reports.push(run_one_file(&path, &mut diffs));
    }
    let pass: usize = reports.iter().map(|f| f.pass).sum();
    let fail: usize = reports.iter().map(|f| f.fail).sum::<usize>() + missing;
    let skip: usize = reports.iter().map(|f| f.skip).sum();
    write_diffs(workspace_root, &diffs);
    println!("TOTAL          pass={pass} fail={fail} skip={skip} (list: {list_rel})");
    if fail == 0 {
        return ExitCode::SUCCESS;
    }
    println!("\nFAILING FILES:");
    for f in reports.iter().filter(|f| f.fail > 0) {
        println!("  {} — {}", f.file, f.fail_reasons.join(" | "));
    }
    ExitCode::from(1)
}

/// One corpus file → its report. Shared by the directory walk and the
/// list mode so the two cannot disagree about what "running a file" is.
fn run_one_file(path: &Path, diff_sink: &mut Vec<String>) -> FileReport {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let records = match parser::parse_file(path) {
        Ok(rs) => rs,
        Err(e) => {
            return FileReport {
                file: file_name,
                pass: 0,
                fail: 1,
                skip: 0,
                fail_reasons: vec![format!("parse: {e}")],
            };
        }
    };
    let mut runner = Runner::new();
    let outcome = runner.run(&records);
    // r1052 (S2.4) — cleanup discipline: name what the file left behind.
    // 7.38.1 S4.2 — the ratchet: the corpus reached zero leftovers, so
    // a leak is now a RED, not a shrug. A yellow warning that survives
    // 210 files is how the pile grew in the first place (the r1020
    // lesson: an unclassified diff line is a bug report nobody read).
    let leaks = runner.leftover_objects();
    if !leaks.is_empty() {
        println!(
            "leak: {} left {}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            leaks.join(", ")
        );
    }
    for d in &outcome.diffs {
        diff_sink.push(format!("=== {}\n{d}", path.display()));
    }
    let mut fail_reasons = Vec::new();
    for (i, o) in outcome.per_record.iter().enumerate() {
        if let Outcome::Fail(reason) = o {
            if fail_reasons.len() < 3 {
                fail_reasons.push(format!("record {i}: {}", short(reason)));
            }
        }
    }
    // 7.38.1 S4.2 — a leftover object fails the FILE (ratchet).
    let mut fail = outcome.fail;
    if !leaks.is_empty() {
        fail += 1;
        fail_reasons.push(format!("leak: left {}", leaks.join(", ")));
    }
    FileReport {
        file: file_name,
        pass: outcome.pass,
        fail,
        skip: outcome.skip,
        fail_reasons,
    }
}

fn run_group(name: &str, dir: &Path, diff_sink: &mut Vec<String>) -> GroupReport {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read group dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("test"))
        .collect();
    files.sort();

    let mut group = GroupReport {
        name: name.to_string(),
        pass: 0,
        fail: 0,
        skip: 0,
        files: Vec::new(),
    };

    for path in files {
        let report = run_one_file(&path, diff_sink);
        group.pass += report.pass;
        group.fail += report.fail;
        group.skip += report.skip;
        group.files.push(report);
    }

    group
}

fn short(reason: &str) -> String {
    let one_line = reason.replace('\n', " | ");
    if one_line.len() > 160 {
        format!("{}…", &one_line[..160])
    } else {
        one_line
    }
}

fn write_json(path: &Path, groups: &[GroupReport]) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("{\n  \"groups\": [\n");
    for (gi, g) in groups.iter().enumerate() {
        if gi > 0 {
            s.push_str(",\n");
        }
        s.push_str(&format!(
            "    {{ \"name\": \"{}\", \"pass\": {}, \"fail\": {}, \"skip\": {},\n",
            g.name, g.pass, g.fail, g.skip
        ));
        s.push_str("      \"files\": [\n");
        for (fi, f) in g.files.iter().enumerate() {
            if fi > 0 {
                s.push_str(",\n");
            }
            s.push_str(&format!(
                "        {{ \"file\": \"{}\", \"pass\": {}, \"fail\": {}, \"skip\": {},\n",
                f.file, f.pass, f.fail, f.skip
            ));
            s.push_str("          \"fail_reasons\": [");
            for (ri, r) in f.fail_reasons.iter().enumerate() {
                if ri > 0 {
                    s.push_str(", ");
                }
                s.push_str(&format!("\"{}\"", escape_json(r)));
            }
            s.push_str("] }");
        }
        s.push_str("\n      ] }");
    }
    s.push_str("\n  ]\n}\n");
    fs::write(path, s)
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn write_md(path: &Path, groups: &[GroupReport]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut s = String::new();
    writeln!(s, "# SPG conformance baseline").unwrap();
    writeln!(s).unwrap();
    writeln!(s, "Per-corpus pass / fail / skip:").unwrap();
    writeln!(s).unwrap();
    writeln!(s, "| corpus | pass | fail | skip | % pass |").unwrap();
    writeln!(s, "|---|---|---|---|---|").unwrap();
    for g in groups {
        let total = g.pass + g.fail + g.skip;
        let pct = if total == 0 {
            0.0
        } else {
            (g.pass as f64) / (total as f64) * 100.0
        };
        writeln!(
            s,
            "| `{}` | {} | {} | {} | {:.1}% |",
            g.name, g.pass, g.fail, g.skip, pct
        )
        .unwrap();
    }
    writeln!(s).unwrap();

    // Top fail reasons across corpus, clustered by short prefix.
    let mut buckets: BTreeMap<String, usize> = BTreeMap::new();
    for g in groups {
        for f in &g.files {
            for r in &f.fail_reasons {
                let bucket = bucket_for(r);
                *buckets.entry(bucket).or_insert(0) += 1;
            }
        }
    }
    let mut top: Vec<(String, usize)> = buckets.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    if !top.is_empty() {
        writeln!(s, "## Top fail patterns").unwrap();
        writeln!(s).unwrap();
        writeln!(s, "| count | pattern |").unwrap();
        writeln!(s, "|---|---|").unwrap();
        for (k, n) in top.iter().take(15) {
            writeln!(s, "| {} | `{}` |", n, k).unwrap();
        }
        writeln!(s).unwrap();
    }

    writeln!(s, "## Per-file detail").unwrap();
    for g in groups {
        writeln!(s).unwrap();
        writeln!(s, "### `{}/`", g.name).unwrap();
        writeln!(s).unwrap();
        writeln!(s, "| file | pass | fail | skip |").unwrap();
        writeln!(s, "|---|---|---|---|").unwrap();
        for f in &g.files {
            writeln!(s, "| `{}` | {} | {} | {} |", f.file, f.pass, f.fail, f.skip).unwrap();
        }
        for f in &g.files {
            if !f.fail_reasons.is_empty() {
                writeln!(s).unwrap();
                writeln!(
                    s,
                    "<details><summary>`{}` fail snippets</summary>\n",
                    f.file
                )
                .unwrap();
                for r in &f.fail_reasons {
                    writeln!(s, "- {r}").unwrap();
                }
                writeln!(s, "</details>").unwrap();
            }
        }
    }

    fs::write(path, s)
}

/// Cluster a fail reason into a coarse pattern: take the first 5 words or so.
/// Good enough for the v1.2 hot-plan brainstorm.
fn bucket_for(reason: &str) -> String {
    let one = reason.replace('\n', " ");
    one.split_whitespace().take(6).collect::<Vec<_>>().join(" ")
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at this crate's Cargo.toml's dir; go up to
    // the workspace root.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| here.clone())
}

/// r1052 (S2.3) — pg_regress's `regression.diffs` idea: every failing
/// query record, as a readable diff with its file and line, in ONE
/// place. Empty runs remove the file so a stale diff can't outlive its
/// failure.
fn write_diffs(workspace_root: &Path, diffs: &[String]) {
    let dir = workspace_root.join("target/suite");
    let path = dir.join("slt.diffs");
    if diffs.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let _ = fs::create_dir_all(&dir);
    if fs::write(&path, diffs.join("\n")).is_ok() {
        println!("diffs -> {}", path.display());
    }
}
