//! `--record` — rewrite the expected blocks of EXPLICITLY NAMED corpus
//! files from actual output (S2.2; MTR's `--record` idea).
//!
//! Guard rails (design D7, from the r1020 baseline lesson — a recorder
//! that can bulk-accept differences turns bugs into "known
//! differences"):
//!
//! - only files named on the command line are touched, never a
//!   directory sweep;
//! - the suite tiers never pass `--record`;
//! - the rewritten file goes through git like any edit — the human
//!   reviews the diff, which is the whole point of recording.
//!
//! Two sources of truth:
//! - engine mode (default): expectations come from the in-process
//!   engine, rendered and SORTED by the same code the runner compares
//!   with, so a recorded file passes by construction;
//! - `--oracle <PG_URI>` mode: statements and queries run against a
//!   live PostgreSQL via psql, cell per line (`-F '\n'`), for corpus
//!   whose expected output should be PG's answer, not ours.

use crate::parser::{ExpectedQuery, Record, SortMode};
use crate::runner::Runner;
use std::path::Path;
use std::process::Command;

/// Rewrite one file. Returns (queries rewritten, human summary).
///
/// # Errors
/// Parse failures, a statement that errors without `expect-error`
/// (state after it would be wrong, so recording stops), or I/O.
pub fn record_file(path: &Path, oracle: Option<&str>) -> Result<(usize, String), String> {
    let records = crate::parser::parse_file(path).map_err(|e| format!("parse: {e:?}"))?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;

    // Execute in order, collecting the actual cells for each RUNNABLE
    // query record (directive-skipped ones get None and keep their old
    // expected text).
    let mut engine_runner = Runner::new();
    let mut actuals: Vec<Option<Vec<String>>> = Vec::new();
    for rec in &records {
        match rec {
            Record::Halt => break,
            Record::Statement {
                directive,
                sql,
                expect_error,
            } => {
                if directive.skip {
                    continue;
                }
                let r = match oracle {
                    None => engine_runner.exec_statement(sql),
                    Some(uri) => psql_exec(uri, sql),
                };
                match (r, expect_error) {
                    (Ok(()), false) | (Err(_), true) => {}
                    (Err(e), false) => {
                        return Err(format!("statement failed mid-recording ({sql}): {e}"));
                    }
                    (Ok(()), true) => {
                        return Err(format!("expected-error statement succeeded: {sql}"));
                    }
                }
            }
            Record::Query {
                directive,
                sql,
                type_string,
                sort,
                ..
            } => {
                if directive.skip {
                    actuals.push(None);
                    continue;
                }
                let cells = match oracle {
                    None => engine_runner.query_actual(sql, type_string, *sort)?,
                    Some(uri) => {
                        let mut cells = psql_cells(uri, sql)?;
                        apply_sort(&mut cells, type_string, *sort);
                        cells
                    }
                };
                actuals.push(Some(cells));
            }
        }
    }

    // Textual rewrite: the Nth query block in the file pairs with the
    // Nth entry of `actuals`. Everything else is copied verbatim, so
    // comments and layout survive.
    let mut out: Vec<String> = Vec::new();
    let mut lines = text.lines().peekable();
    let mut qi = 0usize;
    let mut rewritten = 0usize;
    while let Some(line) = lines.next() {
        out.push(line.to_string());
        if !line.trim_start().starts_with("query ") {
            continue;
        }
        // Copy SQL lines through the ---- separator.
        let mut saw_sep = false;
        for l in lines.by_ref() {
            out.push(l.to_string());
            if l.trim() == "----" {
                saw_sep = true;
                break;
            }
        }
        let replacement = actuals.get(qi).cloned().flatten();
        qi += 1;
        if !saw_sep {
            continue;
        }
        // Old expected runs to the first blank line (or EOF).
        let mut old: Vec<String> = Vec::new();
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() {
                break;
            }
            old.push((*l).to_string());
            lines.next();
        }
        match replacement {
            Some(cells) => {
                if cells != old {
                    rewritten += 1;
                }
                out.extend(cells);
            }
            None => out.extend(old), // skipped record keeps its text
        }
    }
    let mut body = out.join("\n");
    if text.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(path, body).map_err(|e| format!("write: {e}"))?;
    let n_queries = records
        .iter()
        .filter(|r| matches!(r, Record::Query { .. }))
        .count();
    Ok((
        rewritten,
        format!("{n_queries} query blocks, {rewritten} rewritten"),
    ))
}

fn psql_exec(uri: &str, sql: &str) -> Result<(), String> {
    let out = Command::new("psql")
        .args([
            "--no-psqlrc",
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            uri,
            "-c",
            sql,
        ])
        .output()
        .map_err(|e| format!("psql: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn psql_cells(uri: &str, sql: &str) -> Result<Vec<String>, String> {
    // -F '\n' turns column separators into newlines: cell per line,
    // the sqllogictest layout.
    let out = Command::new("psql")
        .args([
            "--no-psqlrc",
            "-X",
            "-q",
            "-t",
            "-A",
            "-F",
            "\n",
            "-v",
            "ON_ERROR_STOP=1",
            uri,
            "-c",
            sql,
        ])
        .output()
        .map_err(|e| format!("psql: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The runner sorts ACTUAL cells before comparing; a recorded expected
/// must therefore be sorted the same way or the file fails on its
/// first replay.
fn apply_sort(cells: &mut Vec<String>, type_string: &str, sort: SortMode) {
    let ncols = type_string.len().max(1);
    match sort {
        SortMode::NoSort => {}
        SortMode::ValueSort => cells.sort(),
        SortMode::RowSort => {
            let mut rows: Vec<Vec<String>> = cells.chunks(ncols).map(<[String]>::to_vec).collect();
            rows.sort();
            *cells = rows.into_iter().flatten().collect();
        }
    }
}

/// Compile-time reminder that hashed expected blocks are not
/// recordable; the parser gives them their own variant.
#[allow(dead_code)]
fn unrecordable(e: &ExpectedQuery) -> bool {
    !matches!(e, ExpectedQuery::Values(_))
}
