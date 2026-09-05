//! `--resume` — carry forward the steps that already passed on this
//! exact tree.
//!
//! A tier runs its steps serially and a failure skips the rest, so a
//! red in a late step costs a full re-run of every green one before it.
//! Measured over the 7.40.0-7.40.6 releases, twenty `prerelease` runs on
//! the testbed: eleven red, 10.1 hours of wall clock. Three of the reds
//! were `perf-sweep`, which is step seven of nine — each one threw away
//! the 1,300 s that had already gone green ahead of it, and two more
//! were `release-build` losing its artefacts to the machine's own
//! weekly cargo sweep, which is not a property of the code at all.
//!
//! What it answers, and what it does not: "has this exact tree already
//! proved this step", never "would this step pass right now". A step
//! whose inputs are not all in the tree — `oracle-three` needs its
//! docker oracles up, `perf-sweep` needs the machine to itself — can be
//! carried past a machine that has since changed. That is why it is a
//! flag and not the default: it is for retrying a run that went red
//! somewhere else, and a release still gets one clean run with nothing
//! carried.
//!
//! What makes carrying sound is the digest: HEAD, the worktree's delta
//! against it, and the untracked files a build would see. Change one
//! byte of any of them and nothing is carried. The report says which
//! steps were carried and from which run, because a step that reads as
//! `pass` without having run is the same lie as a green that never ran.

use std::collections::BTreeMap;
use std::path::Path;

/// A step taken from an earlier run rather than executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    /// The run it passed in.
    pub runid: String,
    /// What it took THERE. Carried into this run's report as-is: the
    /// number belongs to the work, not to the run that reused it.
    pub ms: u64,
}

/// The manifest a digest is taken over.
///
/// Split out from the git calls so the shape can be tested without a
/// repository: what matters is that every input reaches the digest and
/// that they cannot be confused with one another.
#[must_use]
pub fn manifest(head: &str, delta: &str, others: &str, other_hashes: &str) -> String {
    format!(
        "head {}\ndelta {} bytes\n{}\nothers\n{}\nhashes\n{}\n",
        head.trim(),
        delta.len(),
        delta,
        others.trim(),
        other_hashes.trim()
    )
}

/// A digest of everything a tier run reads out of the working tree.
///
/// Uses git's own hashing rather than a crypto dependency: git is
/// already a hard requirement of every step here, and this is a cache
/// key, not a signature.
///
/// # Errors
/// If git is missing, the directory is not a repository, or HEAD is
/// unborn — in each case the caller runs every step, which is the
/// answer a digest that cannot be taken deserves.
pub fn tree_digest(root: &Path) -> Result<String, String> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    let delta = git(root, &["diff", "HEAD"])?;
    let others = git(root, &["ls-files", "--others", "--exclude-standard"])?;
    let other_hashes = if others.trim().is_empty() {
        String::new()
    } else {
        let mut args = vec!["hash-object", "--"];
        let paths: Vec<&str> = others.lines().filter(|l| !l.is_empty()).collect();
        args.extend(paths.iter().copied());
        git(root, &args)?
    };
    hash_stdin(root, &manifest(&head, &delta, &others, &other_hashes))
}

/// Steps that need not run again: they passed (or were themselves
/// carried) in an earlier run of this tier over the same digest.
///
/// Newest verdict wins. A step that passed in one run and failed in a
/// later one over the same tree is NOT carried — the failure is the
/// current fact about it.
#[must_use]
pub fn carried(target_dir: &Path, tier: &str, digest: &str) -> BTreeMap<String, Carried> {
    let mut decided: BTreeMap<String, (String, Carried)> = BTreeMap::new();
    for (_, path) in reports_newest_first(target_dir, tier) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if string_field(&text, "\"tree\":").as_deref() != Some(digest) {
            continue;
        }
        let Some(runid) = string_field(&text, "\"runid\":") else {
            continue;
        };
        for line in text.lines() {
            let Some(name) = string_field(line, "\"name\":") else {
                continue;
            };
            let Some(status) = string_field(line, "\"status\":") else {
                continue;
            };
            let ms = u64_field(line, "\"ms\":").unwrap_or(0);
            // A carried step names the run it originally passed in, so
            // a chain of resumes still points at the run that did the
            // work rather than at the one that last echoed it.
            let from = string_field(line, "\"carried_from\":").unwrap_or_else(|| runid.clone());
            decided
                .entry(name)
                .or_insert((status, Carried { runid: from, ms }));
        }
    }
    decided
        .into_iter()
        .filter(|(_, (status, _))| status == "pass" || status == "carried")
        .map(|(name, (_, c))| (name, c))
        .collect()
}

/// Tier reports in this directory, newest first.
fn reports_newest_first(
    target_dir: &Path,
    tier: &str,
) -> Vec<(std::time::SystemTime, std::path::PathBuf)> {
    let dir = target_dir.join("suite");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = format!("report-{tier}-");
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with(&prefix) && f.ends_with(".json"))
        })
        .filter_map(|p| p.metadata().ok()?.modified().ok().map(|m| (m, p)))
        .collect();
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    files
}

/// The first `"…"` value after `key`.
fn string_field(text: &str, key: &str) -> Option<String> {
    let after = text.split(key).nth(1)?;
    let after = after.trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The first integer after `key`.
fn u64_field(text: &str, key: &str) -> Option<u64> {
    let after = text.split(key).nth(1)?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!("git {} exited {}", args.join(" "), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn hash_stdin(root: &Path, text: &str) -> Result<String, String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("git hash-object: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("git hash-object: no stdin")?
        .write_all(text.as_bytes())
        .map_err(|e| format!("git hash-object: write: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git hash-object: {e}"))?;
    if !out.status.success() {
        return Err(format!("git hash-object exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{Carried, carried, manifest};
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spg-resume-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("suite")).expect("mkdir");
        d
    }

    /// One report file, written the way the ledger writes them.
    fn write_report(dir: &std::path::Path, runid: &str, tree: &str, steps: &[(&str, &str, u64)]) {
        let mut s = format!(
            "{{\n  \"tier\": \"prerelease\",\n  \"runid\": \"{runid}\",\n  \"tree\": \"{tree}\",\n  \"steps\": [\n"
        );
        for (i, (name, status, ms)) in steps.iter().enumerate() {
            s.push_str(&format!(
                "    {{\"name\": \"{name}\", \"status\": \"{status}\", \"ms\": {ms}}}"
            ));
            s.push_str(if i + 1 < steps.len() { ",\n" } else { "\n" });
        }
        s.push_str("  ]\n}\n");
        std::fs::write(
            dir.join("suite")
                .join(format!("report-prerelease-{runid}.json")),
            s,
        )
        .expect("write");
        // Reports are ordered by mtime; make each strictly newer.
        std::thread::sleep(std::time::Duration::from_millis(12));
    }

    #[test]
    fn every_input_reaches_the_manifest_and_they_cannot_be_confused() {
        let a = manifest("abc", "diff-body", "one.rs\ntwo.rs", "h1\nh2");
        for needle in ["abc", "diff-body", "one.rs", "two.rs", "h1", "h2"] {
            assert!(a.contains(needle), "{needle} missing from {a}");
        }
        // Moving a byte from one field to its neighbour must change the
        // digest input — otherwise a renamed file could pass for an
        // edited one.
        assert_ne!(
            manifest("ab", "c", "", ""),
            manifest("a", "bc", "", ""),
            "fields must not run together"
        );
    }

    #[test]
    fn a_green_step_on_the_same_tree_is_carried_with_its_own_duration() {
        let d = tmpdir("green");
        write_report(
            &d,
            "r1-aaaaaaa",
            "tree1",
            &[("lint", "pass", 3000), ("release-build", "pass", 607_000)],
        );
        let got = carried(&d, "prerelease", "tree1");
        assert_eq!(
            got.get("release-build"),
            Some(&Carried {
                runid: "r1-aaaaaaa".to_string(),
                ms: 607_000
            })
        );
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn a_different_tree_carries_nothing() {
        let d = tmpdir("other-tree");
        write_report(&d, "r1-aaaaaaa", "tree1", &[("lint", "pass", 3000)]);
        assert!(carried(&d, "prerelease", "tree2").is_empty());
    }

    #[test]
    fn the_step_that_failed_is_not_carried() {
        let d = tmpdir("red");
        write_report(
            &d,
            "r1-aaaaaaa",
            "tree1",
            &[
                ("lint", "pass", 3000),
                ("perf-sweep", "fail", 700_000),
                ("oracle-three", "skipped", 0),
            ],
        );
        let got = carried(&d, "prerelease", "tree1");
        assert!(got.contains_key("lint"));
        assert!(!got.contains_key("perf-sweep"), "a red must run again");
        assert!(!got.contains_key("oracle-three"), "a skip never ran");
    }

    #[test]
    fn the_newest_verdict_wins_over_an_older_green() {
        let d = tmpdir("newest");
        write_report(&d, "r1-aaaaaaa", "tree1", &[("e2e", "pass", 200_000)]);
        write_report(&d, "r2-bbbbbbb", "tree1", &[("e2e", "fail", 210_000)]);
        assert!(
            !carried(&d, "prerelease", "tree1").contains_key("e2e"),
            "a step that has since failed on this tree must run again"
        );
    }

    #[test]
    fn steps_accumulate_across_several_resumed_runs() {
        let d = tmpdir("chain");
        write_report(
            &d,
            "r1-aaaaaaa",
            "tree1",
            &[
                ("lint", "pass", 3000),
                ("release-build", "pass", 607_000),
                ("e2e", "fail", 212_000),
            ],
        );
        write_report(
            &d,
            "r2-bbbbbbb",
            "tree1",
            &[("e2e", "pass", 156_000), ("gates", "fail", 440_000)],
        );
        let got = carried(&d, "prerelease", "tree1");
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            vec!["e2e", "lint", "release-build"],
            "the run before last still counts for what it proved"
        );
        assert!(!got.contains_key("gates"));
    }

    #[test]
    fn a_carried_step_keeps_pointing_at_the_run_that_did_the_work() {
        let d = tmpdir("provenance");
        write_report(&d, "r1-aaaaaaa", "tree1", &[("lint", "pass", 3000)]);
        // A resumed run echoes it, naming its origin.
        std::fs::write(
            d.join("suite").join("report-prerelease-r2-bbbbbbb.json"),
            "{\n  \"runid\": \"r2-bbbbbbb\",\n  \"tree\": \"tree1\",\n  \"steps\": [\n    {\"name\": \"lint\", \"status\": \"carried\", \"ms\": 3000, \"carried_from\": \"r1-aaaaaaa\"}\n  ]\n}\n",
        )
        .expect("write");
        assert_eq!(
            carried(&d, "prerelease", "tree1")
                .get("lint")
                .map(|c| c.runid.as_str()),
            Some("r1-aaaaaaa"),
            "a chain of resumes must not lose the run that ran it"
        );
    }

    /// The tests above write the JSON by hand, which pins this parser
    /// against my own idea of the ledger's shape rather than against
    /// the ledger. This one goes through the writer.
    #[test]
    fn what_the_ledger_writes_is_what_this_reads() {
        use crate::reportlib::Ledger;
        use std::time::Duration;

        let d = tmpdir("roundtrip");
        let mut l = Ledger::new("prerelease", "20260906T000000-abcdef1");
        l.tree = Some("digest-1".to_string());
        l.record_result(
            "release-build",
            Some(Duration::from_secs(900)),
            Duration::from_millis(607_000),
            true,
        );
        l.record_result("perf-sweep", None, Duration::from_millis(700_000), false);
        l.record_skip("oracle-three");
        l.write(&d).expect("write");

        let got = carried(&d, "prerelease", "digest-1");
        assert_eq!(
            got.get("release-build"),
            Some(&Carried {
                runid: "20260906T000000-abcdef1".to_string(),
                ms: 607_000
            }),
            "the green step, with the duration the ledger recorded"
        );
        assert!(!got.contains_key("perf-sweep"));
        assert!(!got.contains_key("oracle-three"));
        assert!(carried(&d, "prerelease", "digest-2").is_empty());

        // And a resumed run's own report must be readable in turn, or a
        // second resume loses everything the first one carried.
        let mut l2 = Ledger::new("prerelease", "20260906T010000-abcdef1");
        l2.tree = Some("digest-1".to_string());
        l2.record_carried(
            "release-build",
            Duration::from_millis(607_000),
            "20260906T000000-abcdef1",
        );
        l2.record_result("perf-sweep", None, Duration::from_millis(690_000), true);
        std::thread::sleep(std::time::Duration::from_millis(12));
        l2.write(&d).expect("write 2");

        let again = carried(&d, "prerelease", "digest-1");
        assert_eq!(
            again.get("release-build").map(|c| c.runid.as_str()),
            Some("20260906T000000-abcdef1"),
            "a chain of resumes keeps naming the run that did the work"
        );
        assert!(
            again.contains_key("perf-sweep"),
            "the step the resumed run itself proved"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The digest is the whole soundness argument, and the tests above
    /// never touch it — they hand `carried` a string. This one drives
    /// the git plumbing over a real repository: if any of these edits
    /// did not move the digest, a resumed run would carry a step past
    /// the change that invalidated it.
    #[test]
    fn one_byte_anywhere_moves_the_digest() {
        use super::tree_digest;

        let d = tmpdir("digest");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&d)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "suite@example.invalid"]);
        git(&["config", "user.name", "suite"]);
        std::fs::write(d.join("a.txt"), "one\n").expect("write");
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "first"]);

        let base = tree_digest(&d).expect("digest");
        assert_eq!(
            base,
            tree_digest(&d).expect("again"),
            "same tree, same digest"
        );

        // A tracked file edited in the worktree.
        std::fs::write(d.join("a.txt"), "two\n").expect("write");
        let edited = tree_digest(&d).expect("digest");
        assert_ne!(edited, base, "an edit to a tracked file must move it");

        // Put it back: the digest has to come back too, or a resume
        // could never carry anything across a revert.
        std::fs::write(d.join("a.txt"), "one\n").expect("write");
        assert_eq!(
            tree_digest(&d).expect("digest"),
            base,
            "reverting restores it"
        );

        // A file git does not track yet — a new test file is exactly
        // this, and it is what a tier would compile.
        std::fs::write(d.join("b.txt"), "new\n").expect("write");
        let with_new = tree_digest(&d).expect("digest");
        assert_ne!(with_new, base, "an untracked file must move it");

        // And its CONTENT, not only its name.
        std::fs::write(d.join("b.txt"), "different\n").expect("write");
        assert_ne!(
            tree_digest(&d).expect("digest"),
            with_new,
            "an untracked file's contents must move it too"
        );

        // A commit on top of the same worktree state.
        std::fs::remove_file(d.join("b.txt")).expect("rm");
        git(&["commit", "-qam", "second", "--allow-empty"]);
        assert_ne!(
            tree_digest(&d).expect("digest"),
            base,
            "a new HEAD over the same files is a different tree to judge"
        );

        // Ignored files are not part of it: `target/` churns on every
        // run and would make the digest useless.
        std::fs::write(d.join(".gitignore"), "ignored/\n").expect("write");
        git(&["add", ".gitignore"]);
        git(&["commit", "-qm", "ignore"]);
        let with_ignore = tree_digest(&d).expect("digest");
        std::fs::create_dir_all(d.join("ignored")).expect("mkdir");
        std::fs::write(d.join("ignored").join("x"), "churn\n").expect("write");
        assert_eq!(
            tree_digest(&d).expect("digest"),
            with_ignore,
            "build output must not move the digest, or nothing is ever carried"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_report_without_a_tree_is_from_before_this_existed_and_is_ignored() {
        let d = tmpdir("no-tree");
        std::fs::write(
            d.join("suite").join("report-prerelease-old.json"),
            "{\n  \"runid\": \"old\",\n  \"steps\": [\n    {\"name\": \"lint\", \"status\": \"pass\", \"ms\": 3000}\n  ]\n}\n",
        )
        .expect("write");
        assert!(carried(&d, "prerelease", "tree1").is_empty());
    }

    #[test]
    fn another_tiers_reports_are_not_read() {
        let d = tmpdir("tier");
        std::fs::write(
            d.join("suite").join("report-precommit-r1.json"),
            "{\n  \"runid\": \"r1\",\n  \"tree\": \"tree1\",\n  \"steps\": [\n    {\"name\": \"lint\", \"status\": \"pass\", \"ms\": 1}\n  ]\n}\n",
        )
        .expect("write");
        assert!(carried(&d, "prerelease", "tree1").is_empty());
    }
}
