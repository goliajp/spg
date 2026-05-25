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
    let corpus = workspace_root.join("xtests/sqllogictest/corpus");
    let report_json = workspace_root.join("xtests/sqllogictest/report.json");
    let report_md = workspace_root.join("xtests/sqllogictest/report.md");

    if !corpus.is_dir() {
        eprintln!("sqllogictest: corpus dir not found: {}", corpus.display());
        return ExitCode::from(1);
    }

    let mut groups: Vec<GroupReport> = Vec::new();
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
        groups.push(run_group(&group_name, &path));
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

    ExitCode::SUCCESS
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

fn run_group(name: &str, dir: &Path) -> GroupReport {
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
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let records = match parser::parse_file(&path) {
            Ok(rs) => rs,
            Err(e) => {
                // Parse error itself counts as a fail at the file level.
                group.fail += 1;
                group.files.push(FileReport {
                    file: file_name,
                    pass: 0,
                    fail: 1,
                    skip: 0,
                    fail_reasons: vec![format!("parse: {e}")],
                });
                continue;
            }
        };
        let mut runner = Runner::new();
        let outcome = runner.run(&records);
        let mut fail_reasons = Vec::new();
        for (i, o) in outcome.per_record.iter().enumerate() {
            if let Outcome::Fail(reason) = o {
                if fail_reasons.len() < 3 {
                    fail_reasons.push(format!("record {i}: {}", short(reason)));
                }
            }
        }
        group.pass += outcome.pass;
        group.fail += outcome.fail;
        group.skip += outcome.skip;
        group.files.push(FileReport {
            file: file_name,
            pass: outcome.pass,
            fail: outcome.fail,
            skip: outcome.skip,
            fail_reasons,
        });
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
