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

/// Entry point used by `Cmd::Run` and `Cmd::All`. With `bless`, the
/// oracle side is executed live (docker) and its raw output written
/// as the `expected/<stem>.<suffix>.out` baseline instead of compared.
/// v7.38.14 — which collation is the oracle actually answering under?
///
/// Not decoration. The `initdb` pin this release deleted and the per-run
/// `CREATE DATABASE` disagreed, and whichever ran last won — so a corpus
/// expectation silently depended on how old the container was. The run
/// now states its own configuration, which is the cheap version of the
/// rule that every measurement should carry a witness independent of the
/// thing being measured.
///
/// Best-effort: a stack that is down, or a client that changes its output
/// shape, yields `"?"` rather than failing the run. The value is context
/// for a mismatch, never a gate.
fn oracle_collation(oracle: Oracle) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let (container, args, sql): (&str, Vec<&str>, &str) = match oracle {
        Oracle::Pg18 => (
            "spg-oracle-pg18",
            vec![
                "psql", "-X", "-q", "-tA", "-U", "testuser", "-d", "testdb", "-f", "-",
            ],
            "SELECT datcollate FROM pg_database WHERE datname = current_database();",
        ),
        Oracle::Mysql => (
            "spg-oracle-mysql",
            vec!["mysql", "-uroot", "-ptestpass", "-N", "--batch", "testdb"],
            "SELECT @@collation_database;",
        ),
        Oracle::Mariadb => (
            "spg-oracle-mariadb",
            vec!["mariadb", "-uroot", "-ptestpass", "-N", "--batch", "testdb"],
            "SELECT @@collation_database;",
        ),
    };
    let mut cmd = Command::new("docker");
    cmd.arg("exec").arg("-i").arg(container).args(&args);
    let Ok(mut child) = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return "?".into();
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(sql.as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return "?".into();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("?");
    first.trim().to_string()
}

pub fn run_all(corpus: &Path, expected: &Path, oracle: Oracle, bless: bool) -> Result<()> {
    let grouped = naming::list(corpus)?;
    let partition = load_partition(corpus, oracle)?;
    let pipeline = AdjustPipeline::standard();
    let mut total = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for (kind, fixtures) in &grouped {
        if *kind == Kind::Depd {
            // depd.* are setup-only, never run standalone.
            continue;
        }
        for fixture in fixtures {
            let name = fixture
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !partition.contains(name) {
                skipped += 1;
                continue;
            }
            total += 1;
            let outcome = if bless {
                bless_one(fixture, expected, oracle)
            } else {
                run_one(fixture, expected, oracle, &pipeline)
            };
            match outcome {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("FAIL {}: {e:#}", fixture.display());
                    failed += 1;
                }
            }
        }
    }

    println!(
        "oracle={oracle:?} collation={} fixtures={total} failed={failed} skipped_by_partition={skipped}{}",
        oracle_collation(oracle),
        if bless { " [BLESS]" } else { "" }
    );
    if failed > 0 {
        bail!("{failed}/{total} fixtures failed on oracle {oracle:?}");
    }
    Ok(())
}

/// A10 — dialect partition sidecar `ORACLE-<SUFFIX>.list` next to the
/// corpus dir: one fixture filename per line, `#` comments. A listed
/// name that doesn't exist in the corpus is a loud error (a stale
/// list silently narrows coverage otherwise). A fixture NOT listed is
/// skipped for that leg — dialect gating is explicit, never inferred.
fn load_partition(corpus: &Path, oracle: Oracle) -> Result<std::collections::BTreeSet<String>> {
    let list_path = corpus
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("ORACLE-{}.list", oracle.suffix().to_uppercase()));
    let text = std::fs::read_to_string(&list_path)
        .with_context(|| format!("read partition list {}", list_path.display()))?;
    let mut set = std::collections::BTreeSet::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !corpus.join(t).exists() {
            bail!(
                "{}: `{t}` not found in corpus {} — stale entry narrows coverage silently, fix the list",
                list_path.display(),
                corpus.display()
            );
        }
        set.insert(t.to_string());
    }
    Ok(set)
}

/// Capture the live oracle's output as the baseline of record.
fn bless_one(fixture: &Path, expected_root: &Path, oracle: Oracle) -> Result<()> {
    let pair = naming::expected_paths(fixture, expected_root, oracle);
    let raw = run_on_oracle(fixture, oracle)
        .with_context(|| format!("oracle-side execution of {}", fixture.display()))?;
    std::fs::write(&pair.oracle, &raw)
        .with_context(|| format!("write baseline {}", pair.oracle.display()))?;
    println!("BLESS {} <- {} bytes", pair.oracle.display(), raw.len());
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

    let spg_raw = run_on_spg(fixture, oracle)
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
pub fn dump_spg(fixture: &Path, oracle: Oracle) -> Result<String> {
    run_on_spg(fixture, oracle)
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
fn run_on_spg(fixture: &Path, oracle: Oracle) -> Result<String> {
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
    // v7.38.17 — run SPG in the DIALECT of the leg it is being compared
    // against. This function took only the fixture, so every MySQL and
    // MariaDB comparison this differ has ever made ran SPG in
    // PostgreSQL semantics: the instrument whose entire purpose is to
    // compare SPG against those two engines had never put SPG into
    // either one's mode.
    //
    // It is the same failure as `corpus/mysql/` running in PostgreSQL
    // dialect, in the tool built to catch what that corpus missed.
    // v7.39.2 — the MySQL dialect has more than one axis, and this set
    // ONE of them. `"…"` opens a string in a MySQL session and an
    // identifier in a PostgreSQL one, and until the fixture that asks
    // landed, nothing here noticed that the switch was half thrown.
    let mysql_family = matches!(oracle, Oracle::Mysql | Oracle::Mariadb);
    if mysql_family {
        engine.set_mysql_wire_session();
    }
    // v7.37.16 — two-way `SPG_MVCC_INPLACE` override (default is now
    // ON; `=0` runs the differential on the legacy path).
    match std::env::var("SPG_MVCC_INPLACE").ok().as_deref() {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") => {
            engine.set_mvcc_inplace(false);
        }
        Some(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") => {
            engine.set_mvcc_inplace(true);
        }
        _ => {}
    }
    for depd in scan_depd_directives(&body) {
        let depd_path = corpus_dir.join(format!("{depd}.sql"));
        let depd_body = std::fs::read_to_string(&depd_path)
            .with_context(|| format!("read depd {}", depd_path.display()))?;
        for stmt in split_sql_statements(&depd_body, mysql_family) {
            engine
                .execute(&stmt)
                .map_err(|e| anyhow!("depd {depd}: {e:?} (sql: {stmt})"))?;
        }
    }

    // Execute fixture body, capture every Rows result as a psql-style
    // table block; CommandOk results contribute nothing to the diff
    // (they're DDL/DML side-effects with no oracle visible output).
    let mut out_blocks: Vec<String> = Vec::new();
    for stmt in split_sql_statements(&body, mysql_family) {
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
            out_blocks.push(format_rows_psql_aligned(&columns, &rows, mysql_family));
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
fn split_sql_statements(body: &str, backslash_escapes: bool) -> Vec<String> {
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
            // Same rule, same reason — a MySQL-family leg escapes with
            // `\` here too, and `'a\'b'` would have mis-split exactly
            // as the double-quoted form did.
            if c == '\\' && backslash_escapes {
                if let Some(escaped) = chars.next() {
                    cur.push(escaped);
                }
            } else if c == '\'' {
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
            // v7.39.2 — a `"` section ends at a quote that is neither
            // doubled nor escaped.
            //
            // This branch handled neither, while the `'` branch above
            // handled doubling. `SELECT "a\"b" AS x; SELECT …` therefore
            // closed at the escaped quote, reopened at `b"`, and ran to
            // a quote in the NEXT statement — swallowing the `;` and
            // handing the engine two statements as one. The fixture that
            // found it reported a parse error naming the following
            // SELECT, which reads like an engine defect and was the
            // splitter.
            if c == '\\' && backslash_escapes {
                if let Some(escaped) = chars.next() {
                    cur.push(escaped);
                }
            } else if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push(chars.next().unwrap());
                } else {
                    in_double = false;
                }
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
    mysql_family: bool,
) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| spg_value_to_psql_text(v, mysql_family))
                .collect()
        })
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
fn spg_value_to_psql_text(v: &spg_storage::Value<'_>, mysql_family: bool) -> String {
    use spg_storage::Value;
    match v {
        Value::Null => String::new(),
        // v7.39.3 — a boolean is `t`/`f` on the PostgreSQL wire and
        // `1`/`0` on the MySQL one, and SPG's own MySQL wire really does
        // send `1`/`0` (`mysqlwire.rs`). Rendering `t` against a MySQL
        // oracle made the harness misreport SPG rather than compare it,
        // which is why no fixture here could hold a comparison — the one
        // shape a collation fixture is made of. Same class as the differ
        // that ran SPG in PostgreSQL semantics against these two legs.
        Value::Bool(b) => match (*b, mysql_family) {
            (true, true) => "1",
            (false, true) => "0",
            (true, false) => "t",
            (false, false) => "f",
        }
        .to_string(),
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

/// Execute a fixture on a reference master, live, via `docker exec`
/// into the compose stack's container — the engine's own CLI client
/// is the serialiser (psql aligned / mysql --batch), so what lands in
/// `expected/` is the real engine's own words, never a re-rendering.
/// Each fixture gets a schema wipe first: fixtures own their objects.
fn run_on_oracle(fixture: &Path, oracle: Oracle) -> Result<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let body = std::fs::read_to_string(fixture)
        .with_context(|| format!("read fixture {}", fixture.display()))?;
    let corpus_dir = fixture
        .parent()
        .ok_or_else(|| anyhow!("fixture {} has no parent dir", fixture.display()))?;
    let mut sql = String::new();
    for depd in scan_depd_directives(&body) {
        let depd_path = corpus_dir.join(format!("{depd}.sql"));
        sql.push_str(
            &std::fs::read_to_string(&depd_path)
                .with_context(|| format!("read depd {}", depd_path.display()))?,
        );
        sql.push('\n');
    }
    sql.push_str(&body);

    let (container, reset, client): (&str, &str, Vec<&str>) = match oracle {
        Oracle::Pg18 => (
            "spg-oracle-pg18",
            "DROP SCHEMA public CASCADE; CREATE SCHEMA public;",
            vec![
                "psql",
                "-X",
                "-q",
                "-U",
                "testuser",
                "-d",
                "testdb",
                "-P",
                "footer=off",
                "-v",
                "ON_ERROR_STOP=1",
                "-f",
                "-",
            ],
        ),
        // v7.38.14 — the bare CREATE DATABASE inherits the SERVER
        // collation, and that is pinned to `utf8mb4_bin` in
        // `mysql/oracle.cnf` / `mariadb/oracle.cnf`. The pin is
        // deliberate and documented there: byte-wise mirrors PG's
        // C-locale text comparison, so one shared corpus can be compared
        // against all three oracles without a per-fixture folding rule.
        //
        // The consequence, which was NOT written down anywhere and is
        // the reason `oracle_collation` now reports into every run: this
        // harness structurally cannot observe MySQL's DEFAULT
        // (case-insensitive) collation. A fixture that wants that
        // behaviour has to declare it on the column, because the server
        // will not supply it. Two redundant `initdb` ALTER DATABASE
        // scripts that restated the same pin have been deleted — they
        // implied the database was the source of truth, and it is not.
        Oracle::Mysql => (
            "spg-oracle-mysql",
            "DROP DATABASE IF EXISTS testdb; CREATE DATABASE testdb;",
            vec!["mysql", "-uroot", "-ptestpass", "testdb", "--batch"],
        ),
        Oracle::Mariadb => (
            "spg-oracle-mariadb",
            "DROP DATABASE IF EXISTS testdb; CREATE DATABASE testdb;",
            vec!["mariadb", "-uroot", "-ptestpass", "testdb", "--batch"],
        ),
    };

    let exec = |stdin_sql: &str, use_db: bool| -> Result<String> {
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i").arg(container);
        if use_db {
            cmd.args(&client);
        } else {
            // Reset runs without selecting testdb (mysql family drops it).
            match oracle {
                Oracle::Pg18 => {
                    cmd.args([
                        "psql",
                        "-X",
                        "-q",
                        "-U",
                        "testuser",
                        "-d",
                        "testdb",
                        "-v",
                        "ON_ERROR_STOP=1",
                        "-f",
                        "-",
                    ]);
                }
                Oracle::Mysql => {
                    cmd.args(["mysql", "-uroot", "-ptestpass", "--batch"]);
                }
                Oracle::Mariadb => {
                    cmd.args(["mariadb", "-uroot", "-ptestpass", "--batch"]);
                }
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn docker exec (stack up? `spg-oracle-runner docker up`)")?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin_sql.as_bytes())
            .context("feed SQL to oracle client")?;
        let out = child.wait_with_output().context("await oracle client")?;
        if !out.status.success() {
            bail!(
                "oracle client exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    exec(reset, false).context("per-fixture schema reset")?;
    exec(&sql, true)
}
