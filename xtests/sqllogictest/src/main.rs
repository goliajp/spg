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

    let unregistered = unregistered_dialect_dirs(&corpus);
    if !unregistered.is_empty() {
        eprintln!(
            "sqllogictest: corpus dir(s) named after a dialect but not in \
             DIALECT_DIRS: {}. A directory name is a claim; register it so \
             the runner can check its files enter that dialect.",
            unregistered.join(", ")
        );
        return ExitCode::FAILURE;
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

    // v7.39.4 — the SHIPPED-COLLATION panel does not write these.
    //
    // `report.json` and `report.md` are tracked, and they describe the
    // byte-ordering run the corpus was authored under. `gate.sh biz`
    // runs the corpus twice; if the second run wrote them, every gate
    // run would leave a dirty tree and the release check that reads it
    // would be reporting the panel rather than the baseline.
    let collated_panel = std::env::var("SPG_SLT_DB_COLLATION").is_ok_and(|v| !v.is_empty());
    if !collated_panel {
        write_json(&report_json, &groups).expect("write report.json");
        write_md(&report_md, &groups).expect("write report.md");
    }

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
    if collated_panel {
        println!("\n(shipped-collation panel: reports not written)");
    } else {
        println!("\nreport.json -> {}", report_json.display());
        println!("report.md   -> {}", report_md.display());
    }

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
    let mut fail: usize = groups.iter().map(|g| g.fail).sum();
    let skip: usize = groups.iter().map(|g| g.skip).sum();
    write_diffs(&workspace_root, &diffs);

    // v7.38.17 — a coverage assertion about an axis PAIR, not a case.
    //
    // v7.38.16 found four silent wrong answers that all lived where
    // "MySQL semantics" met "an index exists": an indexed join returning
    // the empty set, `s = 'ALPHA'` returning nothing, BETWEEN and
    // ORDER BY LIMIT returning the wrong rows. Across the whole corpus
    // that intersection was EMPTY. The collation fixtures written for
    // 7.38.13 and 7.38.14 exercise comparison paths and never build an
    // index; the files that build indexes never entered MySQL. Not a
    // forgotten case — the two axes had never met, so no amount of
    // adding cases along either one would have found it.
    //
    // Counting cases cannot see that. Counting the intersection can.
    // Each entry is an axis PAIR that a real defect lived in, named with
    // the release that found it. An entry earns its place by having cost
    // something; the registry is not a wish list.
    let axes: &[(&str, &str, fn(&FileReport) -> bool)] = &[
        (
            "mysql-semantics x index-present",
            "v7.38.16: an indexed join returned the empty set; `s = 'ALPHA'` \
             returned nothing; BETWEEN and ORDER BY LIMIT returned the wrong \
             rows. All four needed both axes and the corpus had them in \
             separate files.",
            |f| f.mysql_with_index,
        ),
        (
            "mysql-semantics x CHAR column",
            "v7.38.16: `IN`, `>=` and BETWEEN on a CHAR column kept their \
             case AND their padding, with no index involved — three sites \
             folded Text and not BpChar.",
            |f| f.mysql_with_char,
        ),
    ];
    for (name, why, probe) in axes {
        let n = groups
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| probe(f))
            .count();
        println!("\nAXIS  {name}: {n} file(s)");
        if n == 0 {
            fail += 1;
            println!("  EMPTY — {why}");
            println!(
                "  A corpus can grow along either axis forever and never \
                 cover this. Counting cases does not see it; counting the \
                 intersection does."
            );
        }
    }
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
    /// v7.38.17 — the dialect this file actually ran in, not the one its
    /// directory is named after.
    ///
    /// Those were different for every file under `corpus/mysql/` from the
    /// day that directory was created: the runner had no notion of a
    /// dialect at all, so twenty-one files asserted that MySQL SYNTAX is
    /// accepted and nothing whatsoever about MySQL SEMANTICS. The report
    /// said "21 pass" and meant "21 PostgreSQL runs". Silent wrong
    /// answers on the most ordinary query shapes lived behind that for as
    /// long as the directory existed, and were found by hand instead.
    dialect: &'static str,
    /// v7.38.17 — did this file ever run a statement in MySQL semantics
    /// while an index existed? See `RunOutcome::mysql_with_index`.
    mysql_with_index: bool,
    /// v7.38.17 — see `RunOutcome::mysql_with_char`.
    mysql_with_char: bool,
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
        // The `--list` path honours the same contract: a listed file
        // still has to enter the dialect its directory claims.
        reports.push(run_one_file(
            &path,
            &mut diffs,
            path.parent().and_then(claimed_dialect),
        ));
    }
    let unlisted = unlisted_regressions(workspace_root, list_rel, &text);
    let pass: usize = reports.iter().map(|f| f.pass).sum();
    let fail: usize = reports.iter().map(|f| f.fail).sum::<usize>() + missing + unlisted.len();
    let skip: usize = reports.iter().map(|f| f.skip).sum();
    write_diffs(workspace_root, &diffs);
    for f in &unlisted {
        eprintln!("sqllogictest: regression fixture not in {list_rel}: {f}");
    }
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
/// What dialect a corpus directory's NAME claims its files run in.
///
/// v7.38.17 — a directory name is an assertion, and until this table
/// existed nothing checked it. `corpus/mysql/` held twenty-one files and
/// every one of them ran in PostgreSQL dialect; the report counted them
/// as passing MySQL coverage. Four silent wrong answers on the most
/// ordinary query shapes -- an indexed join returning the empty set
/// among them -- lived behind that and were found by hand in v7.38.16.
///
/// A directory listed here REQUIRES each of its files to declare the
/// matching `dialect` line. A file that does not is a failure, not a
/// skip: a skip is how this grew in the first place.
///
/// Adding a directory named after a mode means adding it here. That is
/// the point — the table is the place the claim gets written down.
const DIALECT_DIRS: &[(&str, &str)] = &[("mysql", "mysql"), ("mariadb", "mariadb")];

/// Every word the `dialect` directive accepts. A corpus directory named
/// after one of these is making a claim, so it has to be registered in
/// [`DIALECT_DIRS`] — otherwise the next `corpus/postgres/` repeats
/// `corpus/mysql/`'s history with a different noun.
const DIALECT_WORDS: &[&str] = &["mysql", "mariadb", "postgres", "postgresql", "pg"];

/// Refuse a corpus directory that names a dialect without registering it.
///
/// Returns the offenders. The caller fails the run rather than warning:
/// a yellow line is what let twenty-one files sit in the wrong mode for
/// as long as they did.
fn unregistered_dialect_dirs(corpus: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(corpus) else {
        return out;
    };
    for e in entries.filter_map(Result::ok) {
        if !e.path().is_dir() {
            continue;
        }
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if DIALECT_WORDS.contains(&name.as_str()) && !DIALECT_DIRS.iter().any(|(d, _)| *d == name) {
            out.push(name);
        }
    }
    out
}

/// The dialect `dir` claims, if it claims one.
fn claimed_dialect(dir: &Path) -> Option<&'static str> {
    let name = dir.file_name().and_then(|s| s.to_str())?;
    DIALECT_DIRS
        .iter()
        .find(|(d, _)| *d == name)
        .map(|(_, want)| *want)
}

/// Name the dialects a run actually visited, from what the engine
/// reported after each record.
///
/// v7.38.17 — this used to read the `dialect` directive, which was the
/// same mistake in a new place: `SET sql_mode = 'STRICT_TRANS_TABLES'`
/// puts a session into MySQL semantics with no directive in sight, and
/// six corpus files entered MySQL exactly that way while a directive-
/// reading report called them PostgreSQL. Observe, do not parse.
fn observed_dialect(o: &sqllogictest::RunOutcome) -> &'static str {
    match (o.entered_mysql, o.entered_postgres) {
        (true, true) => "mixed",
        (true, false) => "mysql",
        _ => "postgres",
    }
}

fn run_one_file(path: &Path, diff_sink: &mut Vec<String>, claims: Option<&str>) -> FileReport {
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
                dialect: "unparsed",
                mysql_with_index: false,
                mysql_with_char: false,
                fail_reasons: vec![format!("parse: {e}")],
            };
        }
    };
    let mut runner = Runner::new();
    let outcome = runner.run(&records);
    let entered = observed_dialect(&outcome);
    // r1052 (S2.4) — cleanup discipline: name what the file left behind.
    // 7.38.1 S4.2 — the ratchet: the corpus reached zero leftovers, so
    // a leak is now a RED, not a shrug. A yellow warning that survives
    // 210 files is how the pile grew in the first place (the r1020
    // lesson: an unclassified diff line is a bug report nobody read).
    // The directory's claim. Checked as an EXTRA failure rather than
    // instead of running the file: the assertions inside are real
    // coverage of something, and dropping them to punish the filing
    // would trade one blind spot for another. What the file is not is
    // coverage of the dialect its directory is named after, and that is
    // what this says.
    let mut claim_broken: Option<String> = None;
    if let Some(want) = claims
        && !(want == "mysql" && outcome.entered_mysql)
    {
        claim_broken = Some(format!(
            "dialect: filed under {want}/ but never enters {want} semantics \
             (observed: {entered}) — add a `dialect {want}` line, or move \
             the file out of a directory whose name claims something it \
             does not test"
        ));
    }
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
    if let Some(reason) = claim_broken {
        fail += 1;
        fail_reasons.push(reason);
    }
    FileReport {
        file: file_name,
        pass: outcome.pass,
        fail,
        skip: outcome.skip,
        dialect: entered,
        mysql_with_index: outcome.mysql_with_index,
        mysql_with_char: outcome.mysql_with_char,
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

    let claims = claimed_dialect(dir);
    for path in files {
        let report = run_one_file(&path, diff_sink, claims);
        group.pass += report.pass;
        group.fail += report.fail;
        group.skip += report.skip;
        group.files.push(report);
    }

    group
}

fn short(reason: &str) -> String {
    let one_line = reason.replace('\n', " | ");
    if one_line.chars().count() > 160 {
        // Count characters, not bytes. `&one_line[..160]` panics the moment
        // byte 160 lands inside a multi-byte character, and this string is
        // only ever built to report a failure -- the panic would replace
        // the failure being reported with one of its own. The same defect
        // was found the same day in `xtests/suitelib/src/steps.rs` and
        // `xtests/dogfood_replay/src/bench.rs`.
        format!("{}…", one_line.chars().take(160).collect::<String>())
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
                "        {{ \"file\": \"{}\", \"pass\": {}, \"fail\": {}, \"skip\": {}, \"dialect\": \"{}\",\n",
                f.file, f.pass, f.fail, f.skip, f.dialect
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

/// What dialects a group's files actually ran in, counted.
///
/// v7.38.17 — the summary line is where "21 mysql files, 21 pass" was
/// read as MySQL coverage for as long as `corpus/mysql/` existed, while
/// every one of those runs was PostgreSQL. A total that cannot show the
/// mode is a total that can hide it.
fn dialect_mix(files: &[FileReport]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for f in files {
        *counts.entry(f.dialect).or_insert(0) += 1;
    }
    counts
        .iter()
        .map(|(d, n)| format!("{d} × {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_md(path: &Path, groups: &[GroupReport]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut s = String::new();
    writeln!(s, "# SPG conformance baseline").unwrap();
    writeln!(s).unwrap();
    writeln!(s, "Per-corpus pass / fail / skip:").unwrap();
    writeln!(s).unwrap();
    writeln!(s, "| corpus | pass | fail | skip | % pass | ran in |").unwrap();
    writeln!(s, "|---|---|---|---|---|---|").unwrap();
    for g in groups {
        let total = g.pass + g.fail + g.skip;
        let pct = if total == 0 {
            0.0
        } else {
            (g.pass as f64) / (total as f64) * 100.0
        };
        writeln!(
            s,
            "| `{}` | {} | {} | {} | {:.1}% | {} |",
            g.name,
            g.pass,
            g.fail,
            g.skip,
            pct,
            dialect_mix(&g.files)
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
        writeln!(s, "| file | pass | fail | skip | ran in |").unwrap();
        writeln!(s, "|---|---|---|---|---|").unwrap();
        for f in &g.files {
            writeln!(
                s,
                "| `{}` | {} | {} | {} | {} |",
                f.file, f.pass, f.fail, f.skip, f.dialect
            )
            .unwrap();
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

/// v7.39.4 — the list's own header says "all of 15_regressions", and
/// until this function existed nothing checked it.
///
/// Measured when the shipped-collation panel was added: the file named
/// ONE of the two `15_regressions` directories. Forty-three fixtures —
/// every regression written since v7.38.13, including the ones this
/// version added — had never run at precommit at all. They were being
/// written, committed, and counted, and the tier whose stated reason to
/// exist is regressions ran none of them.
///
/// Same rule as [`DIALECT_DIRS`] one screen down: a name is an
/// assertion, so something has to compare it against the tree. A
/// fixture missing from the list is a FAILURE and not a warning —
/// a warning is how forty-three of them accumulated.
///
/// Applies to the precommit list only; a hand-written `--list` is a
/// question about specific files and makes no such claim.
fn unlisted_regressions(workspace_root: &Path, list_rel: &str, text: &str) -> Vec<String> {
    if !list_rel.ends_with("PRECOMMIT.list") {
        return Vec::new();
    }
    let listed: std::collections::HashSet<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut missing = Vec::new();
    let mut stack = vec![workspace_root.join("xtests/sqllogictest/corpus")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let here = dir.file_name().and_then(|n| n.to_str()) == Some("15_regressions");
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if here && p.extension().and_then(|x| x.to_str()) == Some("test") {
                let rel = p
                    .strip_prefix(workspace_root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned();
                if !listed.contains(rel.as_str()) {
                    missing.push(rel);
                }
            }
        }
    }
    missing.sort();
    missing
}

#[cfg(test)]
mod short_tests {
    use super::short;

    /// `short` truncated by byte offset, so a failure reason carrying a
    /// multi-byte character across byte 160 panicked the runner instead
    /// of printing the failure it was called to describe. The corpus
    /// does contain multi-byte SQL -- the collation files are full of
    /// it -- so this was reachable, not theoretical.
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
