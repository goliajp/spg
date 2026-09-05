//! What must be true of the MACHINE before a tier's colour means
//! anything.
//!
//! Two failures cost four `prerelease` runs across the 7.40.0-7.40.6
//! releases, and neither was a property of the code:
//!
//! - **A cargo sweep deleting the artefacts underneath the build.**
//!   This machine runs its own weekly cargo hygiene; while it is
//!   sweeping this repository, `release-build` dies with `error: extern
//!   location for pem does not exist: …/libpem-….rmeta`. Twice, at 94 s
//!   and 348 s in. The message names a crate nobody touched, which is
//!   the worst direction for it to point.
//!
//! - **Two tier runs at once.** A detached waiter left over from an
//!   earlier session started a second `prerelease` seven seconds after
//!   one was already going. They shared the target directory, the suite
//!   port range and the test data directories, and the result read as a
//!   genuine defect: `connect: wire: unexpected - in startup` and
//!   `durability append failed: No such file or directory`, with
//!   perm-runner at `server_simple pass=122 fail=5` — while the e2e step
//!   in the same log was 50/50 green over 9,190 tests. The giveaway was
//!   two report files eight seconds apart.
//!
//! The sweep is a scheduled job of the machine's owner and is not ours
//! to kill; the answer is to wait for it and say so. The second run is
//! ours and the answer is to refuse it.

use std::path::{Path, PathBuf};

/// Why a tier must not start yet, or `None` if nothing is in the way.
///
/// Pure over the two things the caller reads from the system, so the
/// decision can be tested without a sweep in progress: `ps` output (one
/// command line per line) and the tail of the machine's cargo-clean log.
///
/// `cargo-sweep-stale` names its repository in argv, so that one is
/// exact. `cargo-clean-weekly` does not — it walks every checkout in
/// turn and names the current one only in its log, so the last unit it
/// announced is what says whether it is our problem right now.
#[must_use]
pub fn sweeper_blocking(ps_output: &str, clean_log: &str, repo: &str) -> Option<String> {
    for line in ps_output.lines() {
        if line.contains("cargo-sweep-stale") && line.contains(repo) {
            return Some(format!("cargo-sweep-stale.sh is sweeping {repo}"));
        }
    }
    if ps_output.lines().any(|l| l.contains("cargo-clean-weekly")) {
        // The last unit it announced. Both shapes name a path:
        //   ACTIVE sweep-stale: /path (NNN MB)
        //   IDLE  (N d, NNN MB) full clean: /path
        let last = clean_log
            .lines()
            .rfind(|l| l.contains("ACTIVE sweep-stale:") || l.contains("full clean:"));
        if let Some(l) = last
            && l.contains(repo)
        {
            return Some(format!("cargo-clean-weekly.sh is on {repo}"));
        }
    }
    None
}

/// Block until no sweep is touching `repo`, printing why once.
///
/// Returns the seconds waited. Polling rather than a notification
/// because these are someone else's `launchd` jobs with no handle to
/// hold; thirty seconds is far below the cost of the failure it avoids.
pub fn wait_out_sweeper(repo: &str) -> u64 {
    let mut waited = 0u64;
    loop {
        let ps = run("ps", &["-ax", "-o", "command="]);
        let log = tail_clean_log();
        let Some(why) = sweeper_blocking(&ps, &log, repo) else {
            if waited > 0 {
                println!("suite: sweep finished after {waited}s — starting");
            }
            return waited;
        };
        if waited == 0 {
            println!("suite: {why} — waiting for it to finish.");
            println!(
                "       It is a scheduled job of this machine's, not ours to kill. \
                 Building through it fails as `extern location for <crate> does not exist`, \
                 which names a crate nobody touched."
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
        waited += 30;
    }
}

fn tail_clean_log() -> String {
    let Ok(home) = std::env::var("HOME") else {
        return String::new();
    };
    let path = Path::new(&home).join("Library/Logs/cargo-clean-weekly.log");
    let Ok(all) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    // Only the tail matters and the file grows without bound.
    let keep: Vec<&str> = all.lines().rev().take(200).collect();
    keep.into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn run(prog: &str, args: &[&str]) -> String {
    std::process::Command::new(prog)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Paths a run dirtied that it found clean.
///
/// Parsed from `git status --porcelain` either side of the run. A tree
/// that was ALREADY dirty going in is the operator's business — this
/// answers only "did the run change something it should have put back".
///
/// v7.40.7 — because the list of files to put back was written twice
/// and the two copies disagreed, so every prerelease left
/// `xtests/dump_compat/report.md` modified while the code that did it
/// carried a comment about how a dirty tree once broke a release
/// preflight. A list that has to be right is worse than a check that
/// says when it is not.
#[must_use]
pub fn newly_dirty(before: &str, after: &str) -> Vec<String> {
    let paths = |s: &str| -> std::collections::BTreeSet<String> {
        s.lines()
            .filter(|l| l.len() > 3)
            .map(|l| {
                let rest = &l[3..];
                // A rename prints `R  old -> new`; the new name is the
                // one that is now on disk.
                rest.rsplit(" -> ")
                    .next()
                    .unwrap_or(rest)
                    .trim()
                    .to_string()
            })
            .collect()
    };
    let b = paths(before);
    paths(after)
        .into_iter()
        .filter(|p| !b.contains(p))
        .collect()
}

/// Exclusive right to run a tier in this working copy.
///
/// Held as a directory, because `mkdir` is the atomic create: two
/// launches in the same millisecond cannot both win it, which a
/// test-then-create on a file cannot promise.
#[derive(Debug)]
pub struct RunLock {
    dir: PathBuf,
}

impl RunLock {
    /// Take the lock, or say who has it.
    ///
    /// A lock whose owner is gone (killed run, rebooted machine) is
    /// cleared and reported — a stale lock that blocks every future run
    /// is a worse failure than the collision it was guarding against.
    ///
    /// # Errors
    /// If a live run holds it, or the directory cannot be created.
    pub fn acquire(target_dir: &Path, tier: &str, pid: u32) -> Result<Self, String> {
        let dir = target_dir.join("suite").join(".running");
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        for attempt in 0..2 {
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    let owner = format!(
                        "pid {pid}\ntier {tier}\nhost {}\n",
                        run("hostname", &["-s"]).trim()
                    );
                    let _ = std::fs::write(dir.join("owner"), owner);
                    return Ok(Self { dir });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let owner = std::fs::read_to_string(dir.join("owner")).unwrap_or_default();
                    let held = owner_pid(&owner);
                    match held {
                        Some(p) if pid_alive(p) => {
                            return Err(format!(
                                "a {} run is already in progress here (pid {p}) — \
                                 refusing to start a second one. Two runs share the target \
                                 directory, the suite port range and the test data \
                                 directories, and their failures read as defects.",
                                owner
                                    .lines()
                                    .find_map(|l| l.strip_prefix("tier "))
                                    .unwrap_or("suite")
                            ));
                        }
                        _ => {
                            println!(
                                "suite: clearing a stale lock (owner {} is gone)",
                                held.map_or_else(|| "unknown".to_string(), |p| p.to_string())
                            );
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    }
                }
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
        }
        Err(format!("{}: could not take the lock", dir.display()))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The pid named in an owner file, if it names one.
#[must_use]
pub fn owner_pid(owner: &str) -> Option<u32> {
    owner
        .lines()
        .find_map(|l| l.strip_prefix("pid "))
        .and_then(|p| p.trim().parse().ok())
}

/// Whether that process is still around. `kill(pid, 0)` asks without
/// sending anything.
///
/// `EPERM` counts as alive, and getting that wrong is the whole point
/// of the test below: signal 0 to a process owned by somebody else —
/// `launchd`, or a run started under a different account — returns -1
/// with `EPERM`, which says the process EXISTS and is not ours to
/// signal. Reading that as "gone" would clear a live run's lock and
/// let the second run start, which is the collision this file exists
/// to prevent.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs error checking only; it delivers
    // nothing and cannot affect the target.
    if unsafe { kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::{RunLock, newly_dirty, owner_pid, pid_alive, sweeper_blocking};

    const REPO: &str = "/Users/doracawl/workspace/goliajp/spg-ci";

    #[test]
    fn a_stale_sweep_of_this_repo_blocks() {
        let ps = format!("bash /Users/doracawl/.local/bin/cargo-sweep-stale.sh {REPO} --force\n");
        assert!(sweeper_blocking(&ps, "", REPO).is_some());
    }

    #[test]
    fn a_sweep_of_another_repo_does_not() {
        let ps = "bash /Users/doracawl/.local/bin/cargo-sweep-stale.sh \
                  /Users/doracawl/workspace/goliajp/spg-base --force\n";
        assert_eq!(
            sweeper_blocking(ps, "", REPO),
            None,
            "the sweep that cost two runs was the one on OUR target, not any sweep"
        );
    }

    #[test]
    fn the_weekly_clean_blocks_only_while_it_is_on_us() {
        let ps = "/bin/bash /Users/doracawl/.local/bin/cargo-clean-weekly.sh\n";
        let on_us = format!("[2026-09-06 04:51:34] ACTIVE sweep-stale: {REPO} (241407 MB)\n");
        assert!(sweeper_blocking(ps, &on_us, REPO).is_some());

        let moved_on = format!(
            "[2026-09-06 04:51:34] ACTIVE sweep-stale: {REPO} (241407 MB)\n\
             [2026-09-06 05:40:53] ACTIVE sweep-stale: /Users/doracawl/workspace/goliajp/torajs (73268 MB)\n"
        );
        assert_eq!(
            sweeper_blocking(ps, &moved_on, REPO),
            None,
            "once it has moved on, waiting for the whole walk is waiting for nothing — \
             on 2026-09-06 that walk had another 20 minutes to run over repos we do not build"
        );
    }

    #[test]
    fn a_full_clean_of_this_repo_blocks_too() {
        let ps = "/bin/bash /Users/doracawl/.local/bin/cargo-clean-weekly.sh\n";
        let log = format!("[2026-09-06 04:51:34] IDLE  (21 d, 4096 MB) full clean: {REPO}\n");
        assert!(
            sweeper_blocking(ps, &log, REPO).is_some(),
            "the tier that deletes the whole target/ is the one that matters most"
        );
    }

    #[test]
    fn a_quiet_machine_blocks_nothing() {
        assert_eq!(sweeper_blocking("zsh\nsshd\n", "", REPO), None);
    }

    #[test]
    fn the_log_alone_does_not_block_after_the_job_has_exited() {
        let on_us = format!("[2026-09-06 04:51:34] ACTIVE sweep-stale: {REPO} (241407 MB)\n");
        assert_eq!(
            sweeper_blocking("zsh\n", &on_us, REPO),
            None,
            "the log is a record, not a running process"
        );
    }

    #[test]
    fn a_run_that_dirties_nothing_reports_nothing() {
        assert!(newly_dirty("", "").is_empty());
        assert!(newly_dirty(" M a.rs\n", " M a.rs\n").is_empty());
    }

    #[test]
    fn a_file_the_run_modified_is_named() {
        assert_eq!(
            newly_dirty("", " M xtests/dump_compat/report.md\n"),
            vec!["xtests/dump_compat/report.md".to_string()],
            "this is the one that made --resume carry nothing"
        );
    }

    #[test]
    fn a_tree_that_was_already_dirty_is_the_operators_business() {
        assert!(
            newly_dirty(" M a.rs\n?? b.rs\n", " M a.rs\n?? b.rs\n").is_empty(),
            "running a tier on a working tree is normal; changing it is not"
        );
        assert_eq!(
            newly_dirty(" M a.rs\n", " M a.rs\n M c.rs\n"),
            vec!["c.rs".to_string()]
        );
    }

    #[test]
    fn a_rename_is_named_by_where_the_file_now_is() {
        assert_eq!(
            newly_dirty("", "R  old.rs -> new.rs\n"),
            vec!["new.rs".to_string()]
        );
    }

    #[test]
    fn an_owner_file_names_its_pid() {
        assert_eq!(owner_pid("pid 4321\ntier prerelease\n"), Some(4321));
        assert_eq!(owner_pid("tier prerelease\n"), None);
    }

    #[test]
    fn a_process_we_may_not_signal_still_counts_as_alive() {
        assert!(pid_alive(std::process::id()), "our own process");
        assert!(
            pid_alive(1),
            "launchd is owned by root: kill(1, 0) is EPERM, not ESRCH, and EPERM means \
             the process is THERE. Reading it as gone clears a live run's lock."
        );
        assert!(
            !pid_alive(4_000_000),
            "beyond any pid this machine hands out"
        );
    }

    #[test]
    fn the_second_run_is_refused_and_the_first_keeps_the_lock() {
        let d = std::env::temp_dir().join(format!("spg-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let first = RunLock::acquire(&d, "prerelease", std::process::id()).expect("first");
        let second = RunLock::acquire(&d, "prerelease", std::process::id());
        assert!(second.is_err(), "two runs share the target and the ports");
        assert!(
            second.unwrap_err().contains("already in progress"),
            "the message has to say what to do about it"
        );
        drop(first);
        RunLock::acquire(&d, "prerelease", std::process::id()).expect("after the first finished");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_lock_left_by_a_dead_run_is_cleared_rather_than_blocking_forever() {
        let d = std::env::temp_dir().join(format!("spg-lock-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let dir = d.join("suite").join(".running");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("owner"), "pid 4000000\ntier prerelease\n").expect("write");
        RunLock::acquire(&d, "prerelease", std::process::id())
            .expect("a lock nobody holds must not block the next run");
        let _ = std::fs::remove_dir_all(&d);
    }
}
