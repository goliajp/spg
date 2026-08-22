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
