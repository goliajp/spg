//! v7.38.17 — the registries are enforced now.
//!
//! `xtests/sigil/test-mode-gucs.md` has carried this line since it was
//! written:
//!
//!   > CI lint rejects any `SPG_TEST_*` symbol that does not appear below.
//!
//! Nothing read that file. Every reference to it in the tree is a
//! comment. The claim was untrue for as long as it was there.
//!
//! It had not drifted — eight symbols in the source, eight rows in the
//! table — so it cost nothing. That is worth saying plainly rather than
//! dramatising: this test makes an existing promise true, it does not
//! clean up a mess.
//!
//! The same shape now covers `xtests/sigil/runtime-env.md`, which is the
//! one that matters more: those are the switches a DEPLOYER sets, and 31
//! of the 83 are read by the engine while nothing in this repository
//! ever sets them.
//!
//! This is one instance of the lesson v7.38.16 and v7.38.17 were spent
//! on: a name is a claim, and a claim nothing checks drifts silently.
//! `corpus/mysql/` was the expensive instance — twenty-one files filed
//! under a dialect they never entered, hiding four wrong answers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/spg-engine.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Every `SPG_…` string literal the shipped crates read, split into the
/// test-only ones and the rest.
fn switches_in_source(root: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut test_only = BTreeSet::new();
    let mut runtime = BTreeSet::new();
    let crates_dir = root.join("crates");
    let mut stack = vec![crates_dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                // Only shipped source. Tests and examples SET switches;
                // reading them here would register a switch because
                // something exercises it, which is backwards.
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name == "tests" || name == "examples" || name == "target" {
                    continue;
                }
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            for sym in string_literals_named_spg(&text) {
                if sym.starts_with("SPG_TEST_") {
                    test_only.insert(sym);
                } else {
                    runtime.insert(sym);
                }
            }
        }
    }
    (test_only, runtime)
}

/// `"SPG_SOMETHING"` occurrences — a quoted literal, which is what an
/// env lookup uses. A bare mention in a comment does not count, and that
/// is deliberate: prose about a switch is not a read of it.
fn string_literals_named_spg(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find("\"SPG_") {
        let start = i + pos + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            end += 1;
        }
        let sym = &text[start..end];
        if sym
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            out.push(sym.to_string());
        }
        i = end.max(start + 1);
    }
    out
}

/// Every switch NAME the registry mentions.
///
/// The tables spell a switch in backticks and often with its value —
/// `SPG_TEST_EXPLAIN_NO_COSTS=1` — so take the leading name and stop at
/// the first character a switch name cannot contain. Requiring the whole
/// backtick chunk to be a name is what the first version of this did,
/// and it reported all eight rows missing from a table that listed all
/// eight.
fn registered(root: &Path, rel: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|e| panic!("{rel} must exist and be readable: {e}"));
    let mut out = BTreeSet::new();
    let mut rest = text.as_str();
    while let Some(pos) = rest.find("SPG_") {
        let tail = &rest[pos..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        out.insert(tail[..end].to_string());
        rest = &tail[end.max(1)..];
    }
    out
}

/// v7.38.18 — read the `exercised` column as the table states it, so
/// the claim can be compared with what the repository actually does.
/// Returns `(name, claimed_exercised)` for each row of the table.
fn exercised_claims(root: &Path, rel: &str) -> Vec<(String, bool)> {
    let text = std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|e| panic!("{rel} must exist and be readable: {e}"));
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| `SPG_") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        if cells.len() < 3 {
            continue;
        }
        let name = cells[0].trim().trim_matches('`').to_string();
        let claim = cells[2].trim().trim_matches('*').trim();
        out.push((name, claim.eq_ignore_ascii_case("yes")));
    }
    out
}

fn names_it(line: &str, name: &str) -> bool {
    // Whole-name matches only. `SPG_AUTOVACUUM` is a prefix of
    // `SPG_AUTOVACUUM_NAPTIME_MS`, so a plain `contains` reported three
    // switches as exercised on the strength of a longer switch's name.
    let mut from = 0;
    while let Some(off) = line[from..].find(name) {
        let at = from + off;
        let after = line[at + name.len()..].chars().next();
        let ok = after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if ok {
            return true;
        }
        from = at + name.len();
    }
    false
}

/// Does anything in this repository outside the register itself set or
/// name this switch, in code rather than in prose?
///
/// v7.38.18 — two rules, and both were learned the hard way.
///
/// **Not in prose.** The register's header used to say `exercised`
/// means "the name appears anywhere under crates/*/tests, xtests,
/// scripts or .github", and by that rule a switch named only in a doc
/// comment counted. `SPG_QUERY_TIMEOUT_MS` was the case in point:
/// `e2e_timeouts.rs` opens with `//! - SPG_QUERY_TIMEOUT_MS: a
/// long-running scan is cancelled`, and the test under it never sets
/// the variable — it uses `SET statement_timeout`.
///
/// **Inside `#[cfg(test)]` counts.** Evidence does not only live under
/// `tests/`. `crates/spg-server/src/main.rs` holds `mod env_knob_tests`
/// in the middle of the file, and the switches it pins would have read
/// `no` forever. The region runs by brace depth from the `mod` line
/// following the attribute, and `the_scanner_can_see_both_kinds_of_
/// evidence` below fails if that tracking breaks.
fn exercised_in_repo(root: &Path, name: &str) -> bool {
    fn scan_file(text: &str, name: &str, tests_only: bool) -> bool {
        let mut depth: i32 = -1; // -1 = outside any #[cfg(test)] module
        let mut armed = false;
        for line in text.lines() {
            let t = line.trim_start();
            if tests_only {
                if depth < 0 {
                    if t.starts_with("#[cfg(test)]") {
                        armed = true;
                    } else if !t.is_empty() {
                        // The attribute must open a MODULE, and the
                        // module line must be the very next one. Most
                        // `#[cfg(test)]` in this tree sit on functions
                        // and fields; arming until the next `mod` line
                        // anywhere below opened a region over ordinary
                        // code — `crates/spg-embedded/src/lib.rs` has
                        // eight such attributes before its test module,
                        // and a production `env::var` 1,700 lines later
                        // was read as evidence.
                        depth = i32::from(armed && t.starts_with("mod ")) - 1;
                        armed = false;
                    }
                }
                if depth < 0 {
                    continue;
                }
                depth += line.matches('{').count() as i32;
                depth -= line.matches('}').count() as i32;
                if depth <= 0 {
                    depth = -1;
                }
            }
            if t.starts_with("//") || t.starts_with('#') || t.starts_with('*') {
                continue;
            }
            if names_it(line, name) {
                return true;
            }
        }
        false
    }
    fn scan(dir: &Path, name: &str, tests_only: bool) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for e in entries.flatten() {
            let path = e.path();
            // The register describes itself, and so does this file:
            // reading either back would make a row its own evidence.
            // `SPG_EMBEDDED_CHECKPOINT_BYTES` reported as exercised the
            // moment it was named in an assertion below, which is a
            // scanner agreeing with the sentence that configured it.
            if path.ends_with("sigil") || path.ends_with("e2e_sigil_registry.rs") {
                continue;
            }
            if path.is_dir() {
                if scan(&path, name, tests_only) {
                    return true;
                }
            } else if let Ok(text) = std::fs::read_to_string(&path)
                && scan_file(&text, name, tests_only)
            {
                return true;
            }
        }
        false
    }
    for rel in ["xtests", "scripts", ".github"] {
        if scan(&root.join(rel), name, false) {
            return true;
        }
    }
    let Ok(crates) = std::fs::read_dir(root.join("crates")) else {
        return false;
    };
    for c in crates.flatten() {
        if scan(&c.path().join("tests"), name, false) || scan(&c.path().join("src"), name, true) {
            return true;
        }
    }
    false
}

/// The scanner above decides 83 rows, so it gets its own pins.
///
/// Both directions, both with a named witness. If the `#[cfg(test)]`
/// brace tracking breaks, the first assertion goes false and every
/// switch pinned only by an in-file unit test quietly reads `no`.
#[test]
fn the_scanner_can_see_both_kinds_of_evidence() {
    let root = repo_root();
    assert!(
        exercised_in_repo(&root, "SPG_STATEMENT_TIMEOUT"),
        "SPG_STATEMENT_TIMEOUT is set by `mod env_knob_tests` inside \
         crates/spg-server/src/main.rs — a #[cfg(test)] module in the \
         middle of the file, not at its end"
    );
    assert!(
        exercised_in_repo(&root, "SPG_SQLX_INLINE_BUDGET_MS"),
        "SPG_SQLX_INLINE_BUDGET_MS is set by an ordinary integration test"
    );
    assert!(
        !exercised_in_repo(&root, "SPG_COMMIT_GROUP_MAX"),
        "SPG_COMMIT_GROUP_MAX appears in three test files and in every \
         one of them it is a doc comment; counting it would be counting \
         prose"
    );
    assert!(
        !exercised_in_repo(&root, "SPG_NO_SUCH_SWITCH_ANYWHERE"),
        "a name nothing mentions must not be found"
    );
}

/// The `exercised` column is a measurement, so measure it.
///
/// It was hand-maintained prose until v7.38.18, which means it could
/// say `yes` about a switch nothing ran and nobody would learn. That is
/// the same shape as the header of `test-mode-gucs.md` claiming a CI
/// lint that did not exist.
#[test]
fn the_exercised_column_says_what_the_repository_does() {
    let root = repo_root();
    let rel = "xtests/sigil/runtime-env.md";
    let mut wrong: Vec<String> = Vec::new();
    for (name, claimed) in exercised_claims(&root, rel) {
        let actual = exercised_in_repo(&root, &name);
        if actual != claimed {
            wrong.push(alloc_line(&name, claimed, actual));
        }
    }
    assert!(
        wrong.is_empty(),
        "{rel} disagrees with the repository on {} switch(es):\n{}\n\
         `exercised` means something outside a comment sets or names it \
         under crates/*/tests, xtests, scripts or .github.",
        wrong.len(),
        wrong.join("\n")
    );
}

fn alloc_line(name: &str, claimed: bool, actual: bool) -> String {
    let word = |b: bool| if b { "yes" } else { "no" };
    format!(
        "  {name}: table says {}, repository says {}",
        word(claimed),
        word(actual)
    )
}

#[test]
fn every_test_mode_switch_is_in_the_sigil_index() {
    let root = repo_root();
    let (test_only, _) = switches_in_source(&root);
    let listed = registered(&root, "xtests/sigil/test-mode-gucs.md");
    let missing: Vec<&String> = test_only.iter().filter(|s| !listed.contains(*s)).collect();
    assert!(
        missing.is_empty(),
        "these SPG_TEST_ switches are read by the source and absent from \
         xtests/sigil/test-mode-gucs.md: {missing:?}. That file's header \
         has promised this check since it was written; it exists now, so \
         add the row in the same commit as the switch."
    );
}

#[test]
fn every_runtime_switch_is_in_the_runtime_registry() {
    let root = repo_root();
    let (_, runtime) = switches_in_source(&root);
    let listed = registered(&root, "xtests/sigil/runtime-env.md");
    let missing: Vec<&String> = runtime.iter().filter(|s| !listed.contains(*s)).collect();
    assert!(
        missing.is_empty(),
        "these environment switches are read by shipped code and absent \
         from xtests/sigil/runtime-env.md: {missing:?}. A deployer can set \
         them, so they are part of the interface; add a row naming the \
         read site and whether anything exercises it."
    );
}
