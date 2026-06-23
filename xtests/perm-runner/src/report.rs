//! Minimal hand-rolled JSON writer for `PermReport` — same rationale as
//! the bespoke TOML parser: no `serde`/`serde_json` build cost on the
//! gate.sh fast path.

use std::fs;
use std::io;
use std::path::Path;

use crate::runner::{PermReport, PermStatus};

pub fn write_perm_report(path: &Path, report: &PermReport) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format_perm_report(report))
}

pub fn format_perm_report(report: &PermReport) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"permutation\": \"{}\",\n",
        escape(&report.permutation)
    ));
    s.push_str(&format!(
        "  \"status\": \"{}\",\n",
        match &report.status {
            PermStatus::Ran => "ran".to_string(),
            PermStatus::SkippedPending(msg) => format!("skipped_pending: {}", escape(msg)),
        }
    ));
    s.push_str(&format!("  \"duration_ms\": {},\n", report.duration_ms));
    let (p, f, k) = report.totals();
    s.push_str(&format!(
        "  \"totals\": {{ \"pass\": {p}, \"fail\": {f}, \"skip\": {k} }},\n"
    ));
    s.push_str("  \"fixtures\": [\n");
    for (i, fr) in report.fixtures.iter().enumerate() {
        if i > 0 {
            s.push_str(",\n");
        }
        s.push_str("    { ");
        s.push_str(&format!(
            "\"fixture\": \"{}\", ",
            escape(&fr.fixture.display().to_string())
        ));
        s.push_str(&format!(
            "\"pass\": {}, \"fail\": {}, \"skip\": {}, \"duration_ms\": {}, ",
            fr.pass, fr.fail, fr.skip, fr.duration_ms,
        ));
        s.push_str("\"fail_snippets\": [");
        for (j, snip) in fr.fail_snippets.iter().enumerate() {
            if j > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("\"{}\"", escape(snip)));
        }
        s.push_str("] }");
    }
    s.push_str("\n  ]\n}\n");
    s
}

fn escape(s: &str) -> String {
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
