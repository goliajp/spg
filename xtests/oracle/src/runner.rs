//! Per-fixture orchestrator.
//!
//! 1. Walk corpus, classify each file (port. / orig. / depd.).
//! 2. For each non-`depd` fixture, execute on SPG (embedded path)
//!    and on the chosen oracle via sqlx.
//! 3. Pass both outputs through `AdjustPipeline`, lex-sort, byte
//!    compare.
//! 4. If a `.spg.out` EXPECTED FAILURE lock exists, compare SPG
//!    against the lock instead of the oracle baseline. Lock no
//!    longer failing => the runner errors out, demanding the lock
//!    file be deleted.
//!
//! v7.38 C scaffolding: ships compilation + control flow. The two
//! execution hooks (`run_on_spg` / `run_on_oracle`) are stubs that
//! return `todo!()`, deliberately blocking real `run --oracle …`
//! invocations until P1 wires sqlx + spg-engine embedded driver.

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

use crate::dialect::Oracle;
use crate::naming::{self, Kind};
use crate::normalise::AdjustPipeline;

/// Entry point used by `Cmd::Run` and `Cmd::All`.
pub fn run_all(corpus: &Path, expected: &Path, oracle: Oracle) -> Result<()> {
    let grouped = naming::list(corpus)?;
    let pipeline = AdjustPipeline::standard();
    let mut total = 0usize;
    let mut failed = 0usize;

    for (kind, fixtures) in &grouped {
        if *kind == Kind::Depd {
            // depd.* are setup-only, never run standalone.
            continue;
        }
        for fixture in fixtures {
            total += 1;
            match run_one(fixture, expected, oracle, &pipeline) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("FAIL {}: {e:#}", fixture.display());
                    failed += 1;
                }
            }
        }
    }

    println!(
        "oracle={oracle:?} fixtures={total} failed={failed} ({:.1}% pass)",
        if total == 0 {
            100.0
        } else {
            100.0 * (total - failed) as f64 / total as f64
        }
    );
    if failed > 0 {
        bail!("{failed}/{total} fixtures failed on oracle {oracle:?}");
    }
    Ok(())
}

/// Run one fixture, returning Ok if SPG matches the expected
/// baseline (oracle-side or SPG EXPECTED FAILURE lock).
fn run_one(
    fixture: &Path,
    expected_root: &Path,
    oracle: Oracle,
    pipeline: &AdjustPipeline,
) -> Result<()> {
    let pair = naming::expected_paths(fixture, expected_root, oracle);

    let spg_raw = run_on_spg(fixture)
        .with_context(|| format!("SPG-side execution of {}", fixture.display()))?;
    let spg_canon = pipeline.apply(spg_raw);

    if pair.spg.exists() {
        // EXPECTED FAILURE lock. SPG must match the lock; if it
        // now matches the oracle too, the lock is stale → fail
        // loud so the human deletes it.
        let lock = std::fs::read_to_string(&pair.spg)
            .with_context(|| format!("read SPG lock {}", pair.spg.display()))?;
        let lock_canon = pipeline.apply(lock);
        if spg_canon != lock_canon {
            bail!(
                "SPG output drifted from EXPECTED FAILURE lock {} — \
                 either fix forward (delete the lock if SPG now matches \
                 the oracle) or update the lock if the new behaviour is \
                 intentional",
                pair.spg.display()
            );
        }
        // Check if the lock is now stale (SPG = oracle).
        if pair.oracle.exists() {
            let oracle_baseline = std::fs::read_to_string(&pair.oracle)
                .with_context(|| format!("read oracle baseline {}", pair.oracle.display()))?;
            let oracle_canon = pipeline.apply(oracle_baseline);
            if spg_canon == oracle_canon {
                bail!(
                    "EXPECTED FAILURE lock {} is no longer failing — \
                     delete the lock so the runner enforces the oracle \
                     baseline directly",
                    pair.spg.display()
                );
            }
        }
        return Ok(());
    }

    // No lock — compare against the oracle baseline.
    if !pair.oracle.exists() {
        bail!(
            "no expected baseline at {} (neither .spg.out lock nor \
             {} oracle output). Generate with `spg-oracle-runner \
             run --oracle {oracle:?} --bless` (--bless lands during \
             P1 fill).",
            pair.spg.display(),
            pair.oracle.display(),
        );
    }
    let oracle_baseline = std::fs::read_to_string(&pair.oracle)
        .with_context(|| format!("read oracle baseline {}", pair.oracle.display()))?;
    let oracle_canon = pipeline.apply(oracle_baseline);

    if spg_canon != oracle_canon {
        bail!(
            "SPG output diverged from oracle baseline {}; promote to \
             EXPECTED FAILURE by writing {} or fix forward",
            pair.oracle.display(),
            pair.spg.display(),
        );
    }
    Ok(())
}

/// Dump the canonicalised SPG output for a single fixture. Public
/// entry for the `Cmd::Dump` subcommand. Useful when filling an
/// EXPECTED FAILURE lock or bisecting an `adjust_*()` pipeline diff.
pub fn dump_spg(fixture: &Path) -> Result<String> {
    run_on_spg(fixture)
}

/// Execute a fixture on SPG using the embedded path.
///
/// **v7.38 P1**: lifted out of stub. Build a fresh `Engine`, resolve
/// any `-- oracle: depends depd.X` directives by sourcing the matching
/// `depd.X.sql` from the same corpus dir, then run the fixture's SQL
/// statement-by-statement. Result rows from every SELECT are rendered
/// in the psql-aligned shape (`col1 | col2 \n----+---- \n val1 | val2`)
/// so the existing `AdjustWhitespace` pipeline step collapses any padding
/// differences against the PG oracle baseline.
fn run_on_spg(fixture: &Path) -> Result<String> {
    use spg_engine::{Engine, QueryResult};

    // Resolve depd.* fixtures referenced via `-- oracle: depends X` in
    // the fixture header. The directive is recognised by the runner
    // only — not the SPG parser (which sees it as a comment and skips).
    let body = std::fs::read_to_string(fixture)
        .with_context(|| format!("read fixture {}", fixture.display()))?;
    let corpus_dir = fixture
        .parent()
        .ok_or_else(|| anyhow!("fixture {} has no parent dir", fixture.display()))?;

    let mut engine = Engine::new();
    for depd in scan_depd_directives(&body) {
        let depd_path = corpus_dir.join(format!("{depd}.sql"));
        let depd_body = std::fs::read_to_string(&depd_path)
            .with_context(|| format!("read depd {}", depd_path.display()))?;
        for stmt in split_sql_statements(&depd_body) {
            engine
                .execute(&stmt)
                .map_err(|e| anyhow!("depd {depd}: {e:?} (sql: {stmt})"))?;
        }
    }

    // Execute fixture body, capture every Rows result as a psql-style
    // table block; CommandOk results contribute nothing to the diff
    // (they're DDL/DML side-effects with no oracle visible output).
    let mut out_blocks: Vec<String> = Vec::new();
    for stmt in split_sql_statements(&body) {
        let stmt_trim = stmt.trim();
        if stmt_trim.is_empty() {
            continue;
        }
        let r = engine.execute(&stmt).map_err(|e| {
            anyhow!(
                "SPG embedded execute failed on {}: {e:?}\nsql: {stmt}",
                fixture.display()
            )
        })?;
        if let QueryResult::Rows { columns, rows } = r {
            out_blocks.push(format_rows_psql_aligned(&columns, &rows));
        }
    }
    Ok(out_blocks.join("\n\n"))
}

/// Scan a fixture body for `-- oracle: depends <name>` directives.
/// Directive lines look like `-- oracle: depends depd.setup_users`
/// (case-sensitive prefix `-- oracle: depends `). Order preserved so
/// dependent fixtures source in declared sequence.
fn scan_depd_directives(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let t = line.trim();
            t.strip_prefix("-- oracle: depends ")
                .map(|name| name.trim().to_string())
        })
        .collect()
}

/// Split a SQL body into statement strings on top-level `;`. Tracks
/// single-quote, double-quote, and `--` line-comment / `/* */` block-
/// comment state so semicolons inside strings or comments don't break
/// statements. The split is intentionally simple — the corpus is
/// hand-authored and avoids dollar-quoted bodies for now.
fn split_sql_statements(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = body.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while let Some(c) = chars.next() {
        if in_line_comment {
            cur.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            cur.push(c);
            if c == '*' && chars.peek() == Some(&'/') {
                cur.push(chars.next().unwrap());
                in_block_comment = false;
            }
            continue;
        }
        if in_single {
            cur.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    // '' escape — consume both, stay inside the string
                    cur.push(chars.next().unwrap());
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            cur.push(c);
            if c == '"' {
                in_double = false;
            }
            continue;
        }
        match c {
            '\'' => {
                cur.push(c);
                in_single = true;
            }
            '"' => {
                cur.push(c);
                in_double = true;
            }
            '-' if chars.peek() == Some(&'-') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                in_line_comment = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                in_block_comment = true;
            }
            ';' => {
                let trimmed = cur.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Render rows in a psql `\pset format aligned` shape. The runner's
/// `AdjustWhitespace` step collapses padding so byte-equal compare
/// against the PG oracle baseline survives PG's exact alignment math.
fn format_rows_psql_aligned(
    columns: &[spg_storage::ColumnSchema],
    rows: &[spg_storage::Row<'static>],
) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.values.iter().map(spg_value_to_psql_text).collect())
        .collect();

    // Column widths = max(header.len(), max cell.len()) per col.
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let header_w = c.name.len();
            let cell_w = cells
                .iter()
                .map(|r| r.get(i).map(|s| s.len()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            header_w.max(cell_w)
        })
        .collect();

    let mut out = String::new();
    // Header
    let header: Vec<String> = columns
        .iter()
        .zip(widths.iter())
        .map(|(c, w)| format!(" {:^width$} ", c.name, width = *w))
        .collect();
    out.push_str(&header.join("|"));
    out.push('\n');
    // Separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
    out.push_str(&sep.join("+"));
    out.push('\n');
    // Rows
    for r in &cells {
        let cells_line: Vec<String> = r
            .iter()
            .zip(widths.iter())
            .map(|(v, w)| format!(" {:>width$} ", v, width = *w))
            .collect();
        out.push_str(&cells_line.join("|"));
        out.push('\n');
    }
    out
}

/// Map a single SPG `Value` to its psql-style text form. PG renders
/// `NULL` as empty in aligned mode by default; we follow suit since
/// `AdjustNullDisplay` does its own normalisation downstream.
fn spg_value_to_psql_text(v: &spg_storage::Value<'_>) -> String {
    use spg_storage::Value;
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(f) => {
            // Match PG's default-precision representation. (Both branches
            // currently format the same way; kept as a single arm.)
            format!("{f}")
        }
        Value::Text(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute a fixture on a reference master via sqlx.
///
/// **Stub.** v7.38 C ships the call shape; P1 wires sqlx
/// `query.execute` + dialect-specific result serialiser.
#[allow(dead_code)]
fn run_on_oracle(_fixture: &Path, _oracle: Oracle) -> Result<String> {
    Err(anyhow!(
        "run_on_oracle: stub — wire sqlx adapter (PG/MySQL/MariaDB) \
         during v7.38 P1 fill"
    ))
}
