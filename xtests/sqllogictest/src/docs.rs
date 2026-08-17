//! doc-as-corpus (7.38 S4.5, design D17) — every ```sql fence in the
//! public docs EXECUTES on the embedded engine. Documentation that
//! errors is documentation that lies; the full tier runs this so a
//! doc drifting from the engine turns red like any other fixture.
//!
//! Annotations:
//! - fence info `no-run` (```sql no-run) — an illustrative block that
//!   references a schema the doc never creates (or a feature-gated
//!   surface); counted and reported, never silently ignored.
//! - a `-- expect-error` comment line — the NEXT statement must fail;
//!   its succeeding is a red (the doc documents an error).
//!
//! Each FILE gets a fresh engine with the corpus runner's fixed
//! clock, so doc examples may build on each other within a file but
//! never across files.

use std::path::PathBuf;

struct Block {
    file: String,
    start_line: usize,
    no_run: bool,
    sql: String,
}

fn extract_blocks(file: &str, text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut cur: Option<Block> = None;
    for (ln, line) in text.lines().enumerate() {
        let t = line.trim();
        if let Some(b) = &mut cur {
            if t.starts_with("```") {
                out.push(cur.take().expect("open block"));
            } else {
                b.sql.push_str(line);
                b.sql.push('\n');
            }
            continue;
        }
        if let Some(info) = t.strip_prefix("```sql") {
            cur = Some(Block {
                file: file.to_string(),
                start_line: ln + 1,
                no_run: info.contains("no-run"),
                sql: String::new(),
            });
        }
    }
    out
}

/// Statement splitter — quote/comment aware, top-level `;` only. The
/// docs corpus is hand-written prose SQL; dollar-quoting is out of
/// scope (a doc that needs it should be a .test fixture instead).
fn split_statements(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = body.chars().peekable();
    let (mut in_sq, mut in_dq, mut in_line, mut in_block) = (false, false, false, false);
    while let Some(c) = chars.next() {
        if in_line {
            cur.push(c);
            if c == '\n' {
                in_line = false;
            }
            continue;
        }
        if in_block {
            cur.push(c);
            if c == '*' && chars.peek() == Some(&'/') {
                cur.push(chars.next().expect("peeked"));
                in_block = false;
            }
            continue;
        }
        if in_sq {
            cur.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    cur.push(chars.next().expect("peeked"));
                } else {
                    in_sq = false;
                }
            }
            continue;
        }
        if in_dq {
            cur.push(c);
            if c == '"' {
                in_dq = false;
            }
            continue;
        }
        match c {
            '\'' => {
                cur.push(c);
                in_sq = true;
            }
            '"' => {
                cur.push(c);
                in_dq = true;
            }
            '-' if chars.peek() == Some(&'-') => {
                cur.push(c);
                cur.push(chars.next().expect("peeked"));
                in_line = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                cur.push(c);
                cur.push(chars.next().expect("peeked"));
                in_block = true;
            }
            ';' => {
                if !cur.trim().is_empty() {
                    out.push(cur.clone());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Run the docs corpus. Default file set: `README.md` + `docs/*.md`.
///
/// Returns process exit code semantics: 0 clean, 1 any failure.
pub fn run(paths: &[String]) -> i32 {
    let files: Vec<PathBuf> = if paths.is_empty() {
        let mut v = vec![PathBuf::from("README.md")];
        if let Ok(rd) = std::fs::read_dir("docs") {
            let mut docs: Vec<PathBuf> = rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect();
            docs.sort();
            v.extend(docs);
        }
        v
    } else {
        paths.iter().map(PathBuf::from).collect()
    };

    let (mut n_blocks, mut n_skipped, mut n_stmts, mut failures) = (0usize, 0usize, 0usize, 0usize);
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            eprintln!("docs: cannot read {}", f.display());
            failures += 1;
            continue;
        };
        let blocks = extract_blocks(&f.display().to_string(), &text);
        if blocks.is_empty() {
            continue;
        }
        // Fresh engine per FILE — examples may build within a file.
        let mut engine = spg_engine::Engine::new().with_clock(crate::runner::fixed_test_clock);
        for b in blocks {
            n_blocks += 1;
            if b.no_run {
                n_skipped += 1;
                println!("docs: SKIP (no-run) {}:{}", b.file, b.start_line);
                continue;
            }
            let mut expect_error = false;
            for stmt in split_statements(&b.sql) {
                let trimmed = stmt.trim();
                // The annotation rides as a comment INSIDE the previous
                // statement's tail or on its own; scan this statement's
                // leading comments for it.
                if trimmed
                    .lines()
                    .take_while(|l| l.trim_start().starts_with("--"))
                    .any(|l| l.contains("expect-error"))
                {
                    expect_error = true;
                }
                if trimmed
                    .lines()
                    .all(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
                {
                    continue;
                }
                n_stmts += 1;
                let r = engine.execute(trimmed);
                match (r, expect_error) {
                    (Ok(_), false) => {}
                    (Err(_), true) => {}
                    (Err(e), false) => {
                        eprintln!(
                            "docs: FAIL {}:{}: {e}\n  sql: {}",
                            b.file,
                            b.start_line,
                            trimmed.lines().next().unwrap_or_default()
                        );
                        failures += 1;
                    }
                    (Ok(_), true) => {
                        eprintln!(
                            "docs: FAIL {}:{}: statement annotated expect-error SUCCEEDED\n  sql: {}",
                            b.file,
                            b.start_line,
                            trimmed.lines().next().unwrap_or_default()
                        );
                        failures += 1;
                    }
                }
                expect_error = false;
            }
        }
    }
    println!(
        "docs corpus: files={} blocks={n_blocks} (skipped {n_skipped}) statements={n_stmts} failures={failures}",
        files.len()
    );
    i32::from(failures > 0)
}
