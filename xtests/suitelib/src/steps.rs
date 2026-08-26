//! Internal step implementations for `suite-run` (S0.10).
//!
//! Each returns Ok(summary) on pass, Err(reason) on fail — the ledger
//! does the timing, this module does the work.

use crate::crategraph::CrateGraph;
use crate::proclib::Roster;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn sh(root: &Path, cmd: &str) -> Result<String, String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn `{cmd}`: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`{cmd}` exited {}:\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Workspace crates touched by the uncommitted diff (vs HEAD), mapped
/// through the crate graph's directory table.
///
/// # Errors
/// git failures only; an empty diff is Ok(empty).
pub fn changed_crates(root: &Path, graph: &CrateGraph) -> Result<Vec<String>, String> {
    let diff = sh(root, "git diff --name-only HEAD")?;
    let mut hits: Vec<String> = Vec::new();
    for file in diff.lines() {
        for (name, dir) in &graph.dirs {
            if file.starts_with(dir.as_str()) && !hits.contains(name) {
                hits.push(name.clone());
            }
        }
    }
    hits.sort();
    Ok(hits)
}

/// `clippy-affected` — clippy over the affected closure, debug profile.
///
/// # Errors
/// Clippy findings (the output is the reason).
pub fn clippy_affected(root: &Path, graph: &CrateGraph) -> Result<String, String> {
    let changed = changed_crates(root, graph)?;
    if changed.is_empty() {
        return Ok("no crate changes — skipped".into());
    }
    let affected = graph.affected(&changed);
    let flags: String = affected.iter().map(|c| format!(" -p {c}")).collect();
    sh(root, &format!("cargo clippy -q{flags} -- -D warnings"))?;
    Ok(format!("clippy clean over {} crates", affected.len()))
}

/// `unit-affected` — `--lib --bins` tests over the affected closure.
///
/// # Errors
/// Test failures (the output is the reason).
/// v7.38.22 — the crate selection `unit-affected` will run, and the
/// `-p` flags that name it.
///
/// Shared with the tier's PREPARE phase, and that is the whole point.
/// Prepare warmed the build with `--workspace`, the step then ran
/// `-p A -p B …`, and Cargo resolves features over the SELECTED members:
/// a subset gets a different feature set for the shared dependencies,
/// so nothing prepare built could be reused and the step rebuilt from
/// scratch inside a hard budget.
///
/// What that looked like, same step, same band, five runs recorded in
/// the reports: 2289.9 s, 167.8 s, 1585.8 s, 8.9 s, 505.2 s. A 257x
/// spread on a step whose budget is 480 s — a coin flip, not a gate.
/// The short readings are the ones where the subset happened to be warm
/// from a previous run.
///
/// `Ok(None)` when nothing changed, which is the step's own skip.
///
/// # Errors
/// Whatever `changed_crates` reports.
pub fn affected_selection(
    root: &Path,
    graph: &CrateGraph,
) -> Result<Option<(Vec<String>, String)>, String> {
    let changed = changed_crates(root, graph)?;
    if changed.is_empty() {
        return Ok(None);
    }
    let affected = graph.affected(&changed);
    let flags: String = affected.iter().map(|c| format!(" -p {c}")).collect();
    Ok(Some((affected, flags)))
}

pub fn unit_affected(root: &Path, graph: &CrateGraph) -> Result<String, String> {
    let Some((affected, flags)) = affected_selection(root, graph)? else {
        return Ok("no crate changes — skipped".into());
    };
    // v7.38.2 — cargo errors on `--lib` when a SINGLE selected package
    // has no lib target (bins-only spg-server / spgctl), but silently
    // tolerates the same flags across MULTIPLE packages — so this step
    // only ever failed when exactly one bins-only crate changed. Retry
    // without `--lib` on that precise error.
    //
    // v7.38.22 — `2>&1`, so the step can see whether it SPENT its budget
    // or waited for it.
    //
    // This step has recorded 2289.9 s, 167.8 s, 1585.8 s, 8.9 s and
    // 505.2 s at the same band against a hard 480 s budget. Timed
    // directly, twice: 1017 s wall for 20.4 s user and 10.9 s sys, then
    // 725 s wall for 11.8 s and 1.3 s. Ninety-seven per cent of the
    // budget went on waiting, and what it waited for was another cargo
    // holding the build-directory lock — a full workspace clippy, in the
    // run that produced the 2289 s.
    //
    // Cargo says so on stderr, in as many words. Reading it turns a
    // number nobody could account for into a named cause.
    let blocked = "Blocking waiting for file lock";
    let waited = match sh(root, &format!("cargo test -q{flags} --lib --bins 2>&1")) {
        Ok(out) => out.contains(blocked),
        Err(e) if e.contains("no library targets") => {
            sh(root, &format!("cargo test -q{flags} --bins 2>&1"))?.contains(blocked)
        }
        Err(e) => return Err(e),
    };
    let note = if waited {
        " — and WAITED on the build-directory lock, so this duration is another cargo's"
    } else {
        ""
    };
    Ok(format!("unit green over {} crates{note}", affected.len()))
}

/// `pins-current` — the e2e pins THIS commit adds or touches.
///
/// v7.38.19 — the step used to be `cargo test --test e2e pin_v738_`,
/// a filter on a test-NAME prefix that exactly one test in the repo
/// carries. Meanwhile 33 e2e files with 195 tests had been added since
/// v7.38.0. A step named for this version's pins was running one test
/// and could not go red for any reason a pin exists to catch.
///
/// The selector is now the diff, which is what the tier's siblings
/// already use, and it has two ways to go red that the old one did not:
/// a new pin file that `main.rs` never declares (it would compile,
/// ship, and never run), and a filter set that matches no tests at all.
///
/// # Errors
/// A failing pin, an unwired pin file, or a filter set that selects
/// nothing while pin files were touched.
pub fn pins_current(root: &Path) -> Result<String, String> {
    // `d` excludes deletions: a removed file cannot be run, and naming
    // it as a filter would select nothing and read as the failure below.
    //
    // Tracked files only, which is staged plus unstaged -- the contents
    // of the commit. An UNTRACKED pin file is not in this commit, and a
    // draft that included those made the step red for work the author
    // had not asked to commit yet. A new pin file is seen the moment it
    // is staged, which is the moment it becomes part of a commit.
    let diff = sh(root, "git diff --name-only --diff-filter=d HEAD")?;
    let mut mods: Vec<String> = Vec::new();
    for f in diff.lines() {
        let Some(rest) = f.strip_prefix("crates/spg-engine/tests/e2e/") else {
            continue;
        };
        let Some(m) = rest.strip_suffix(".rs") else {
            continue;
        };
        if m == "main" || m.contains('/') || mods.iter().any(|x| x == m) {
            continue;
        }
        mods.push(m.to_string());
    }
    if mods.is_empty() {
        return Ok("no e2e pins touched — skipped".into());
    }
    // A pin file that `main.rs` does not declare is not compiled into
    // the harness. It reviews as coverage and runs never.
    let main_rs = std::fs::read_to_string(root.join("crates/spg-engine/tests/e2e/main.rs"))
        .map_err(|e| format!("reading the e2e main.rs: {e}"))?;
    let unwired: Vec<&String> = mods
        .iter()
        .filter(|m| !main_rs.contains(&format!("mod {m};")))
        .collect();
    if !unwired.is_empty() {
        return Err(format!(
            "pin file(s) not declared in crates/spg-engine/tests/e2e/main.rs, \
             so nothing in them runs: {unwired:?}"
        ));
    }
    let filters = mods.join(" ");
    let out = sh(
        root,
        &format!("cargo test -q -p spg-engine --test e2e -- {filters}"),
    )?;
    let ran: usize = out
        .lines()
        .filter_map(|l| l.strip_prefix("test result: ok. "))
        .filter_map(|l| l.split(' ').next())
        .filter_map(|n| n.parse::<usize>().ok())
        .sum();
    if ran == 0 {
        return Err(format!(
            "{} pin file(s) touched but the filters selected no tests: {mods:?}",
            mods.len()
        ));
    }
    Ok(format!("{ran} pin(s) over {} touched file(s)", mods.len()))
}

/// `perf-sweep` (S1.2) — the release-blocking endpoint sweep with its
/// legs configured HERE, never from operator env: PERF blocked two
/// release trains for missing URIs before this step existed.
///
/// Environment detection is by artifact, not hostname: the mini
/// testbed carries `~/spgbench/bin/psql` (a docker-exec wrapper whose
/// container reaches the host as `host.docker.internal`); anywhere
/// else uses local psql and 127.0.0.1 for both legs. Both configs keep
/// the two legs on ONE host string — r1022's symmetry rule.
///
/// # Errors
/// Missing PG leg, server failure, or `losses>0` — the verdict line is
/// quoted either way.
pub fn perf_sweep(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let home = std::env::var("HOME").map_err(|_| "no $HOME")?;
    let wrapper = Path::new(&home).join("spgbench/bin/psql");
    let (psql, host, bind) = if wrapper.exists() {
        (
            wrapper.display().to_string(),
            "host.docker.internal",
            "0.0.0.0",
        )
    } else {
        ("psql".to_string(), "127.0.0.1", "127.0.0.1")
    };
    // v7.38.19 — the PostgreSQL leg orders text the way the SPGS leg
    // does, which it did not.
    //
    // The oracle container's `bench` database is `en_US.utf8`; the SPGS
    // leg is `C`. For a text sort those are different work, and the
    // sixty-four cells had been reporting WINS on the difference:
    // `short text distinct` read 0.44x across the collations and 3.01x
    // within one. `bench_c` is the same server, same box, same data,
    // ordering bytes — which is also what `postgres:18-alpine` does for
    // the customers this panel speaks for, musl carrying no locale data.
    //
    // Created here rather than assumed: a step that silently falls back
    // to the mismatched database would put the flattering numbers back.
    let pg_uri = format!("postgres://bench:bench@{host}:25432/bench_c");
    let admin_uri = format!("postgres://bench:bench@{host}:25432/postgres");
    let _ = sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q -tA '{admin_uri}' -c \"SELECT 1 FROM pg_database WHERE \
             datname = 'bench_c'\" | grep -q 1 || {psql} --no-psqlrc -X -q -tA '{admin_uri}' \
             -c \"CREATE DATABASE bench_c TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C' \
             ENCODING 'UTF8'\""
        ),
    );
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-sweep"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    // v7.38.19 — DECLARE the baseline leg's collation instead of
    // inheriting the machine's.
    //
    // The testbed exports `LANG=en_US.UTF-8` and `LC_ALL=en_US.UTF-8`, so
    // this leg has been running under a LOCALE collation while every
    // comment about the sweep says it runs under `C`. Two consequences,
    // and the second is worse than the first:
    //
    //   * the sixty-four cells were measuring the collated path, not the
    //     byte one, and their history is read as the byte one
    //   * the locale panel added earlier in this very version was
    //     comparing en_US against en_US -- **the same thing to itself**
    //     -- and reported `losses=0` for it
    //
    // The panel added to catch a collation regression could not have
    // caught one. `SPG_LC_COLLATE=C` is now explicit, which is the same
    // rule the three-engine differential learned in v7.38.16: state what
    // you are measuring rather than letting the host decide.
    let port = roster.spawn_server_env(
        "sweep-leg",
        &bin,
        &tmp,
        Duration::from_secs(20),
        bind,
        &[("SPG_LC_COLLATE", "C")],
    )?;
    let spg_uri = format!("postgres://bench:bench@{host}:{port}/bench");
    // Both legs must answer before anything is timed (r1041).
    for uri in [&pg_uri, &spg_uri] {
        sh(
            root,
            &format!("{psql} --no-psqlrc -X -q -tA '{uri}' -c 'SELECT 1'"),
        )
        .map_err(|e| format!("leg {uri} not answering: {e}"))?;
    }
    let out = sh(
        root,
        &format!(
            "PSQL='{psql}' PG_URI='{pg_uri}' SPG_URI='{spg_uri}' bash scripts/perf-endpoint-sweep.sh"
        ),
    );
    // v7.38.19 — a second SPG leg, under a LOCALE database collation,
    // measured against the first.
    //
    // Every one of the sixty-four cells above runs under `C`, so the
    // sweep could not see what a declared collation costs -- and what it
    // cost was a factor of twenty-six on `WHERE kind = 'click'` over
    // 200,000 rows, shipped in v7.38.18 and found only when a customer's
    // slowest shape was pulled apart by hand.
    //
    // Comparing SPG-under-a-locale against SPG-under-C rather than
    // against PostgreSQL is deliberate: it is the same binary on the
    // same box in the same window, so the machine's speed cancels and
    // what is left is the question worth asking -- does declaring a
    // collation change the COST CLASS of an ordinary query. The script's
    // own control leg and its refusal to call an unresolved difference
    // both apply unchanged, because it is the same script.
    let locale_out = (|| -> Result<String, String> {
        let tmp2 = crate::proclib::run_tmp_dir(&format!("{runid}-sweep-locale"));
        let _ = std::fs::remove_dir_all(&tmp2);
        let mut roster2 = Roster::new();
        let port2 = roster2.spawn_server_env(
            "sweep-leg-locale",
            &bin,
            &tmp2,
            Duration::from_secs(20),
            bind,
            &[("SPG_LC_COLLATE", "en_US.utf8")],
        )?;
        let locale_uri = format!("postgres://bench:bench@{host}:{port2}/bench");
        sh(
            root,
            &format!("{psql} --no-psqlrc -X -q -tA '{locale_uri}' -c 'SELECT 1'"),
        )
        .map_err(|e| format!("locale leg {locale_uri} not answering: {e}"))?;
        // `SIZES` trimmed to the largest band only: the question is a
        // cost CLASS, which the widest row count answers most clearly,
        // and the whole panel twice would not fit the tier's budget.
        let r = sh(
            root,
            &format!(
                "PSQL='{psql}' PG_URI='{spg_uri}' SPG_URI='{locale_uri}' SIZES=400000 \
                 EXPECT_SPG_COLLATE=en_US.utf8 ALLOW_COLLATION_MISMATCH=1 \
                 SORT_CEILING=2.0 \
                 bash scripts/perf-endpoint-sweep.sh"
            ),
        );
        // v7.38.22 — and a THIRD comparison, against PostgreSQL under the
        // SAME locale.
        //
        // The sixty-four cells above compare `C` against `C`, which was
        // the configuration this image shipped. It is not any more: the
        // image now sets `LANG=en_US.utf8`, the same default
        // `postgres:18` carries, so a new database on either side
        // collates by locale. A panel that only compares byte order no
        // longer measures what a customer runs.
        //
        // The same SPG leg, already up under `en_US.utf8`, against
        // PostgreSQL's `bench` — which is `en_US.utf8`. No
        // `ALLOW_COLLATION_MISMATCH` here: the two legs are supposed to
        // agree, and the script's own check should say so if they stop.
        let shipped_uri = format!("postgres://bench:bench@{host}:25432/bench");
        let shipped = sh(
            root,
            &format!(
                "PSQL='{psql}' PG_URI='{shipped_uri}' SPG_URI='{locale_uri}' SIZES=400000 \
                 EXPECT_SPG_COLLATE=en_US.utf8 bash scripts/perf-endpoint-sweep.sh"
            ),
        );
        roster2.reap_all();
        let _ = std::fs::remove_dir_all(&tmp2);
        // Both verdicts travel; the caller grades them separately.
        match (r, shipped) {
            (Ok(a), Ok(b)) => Ok(format!("{a}\n===SHIPPED-DEFAULT===\n{b}")),
            (Err(e), _) => Err(e),
            (Ok(a), Err(e)) => Ok(format!("{a}\n===SHIPPED-DEFAULT===\nFAILED: {e}")),
        }
    })();
    // D20 — the sweep leg's peak RSS goes into the account, and the
    // manifest ceiling has teeth at reap.
    let ceiling = std::fs::read_to_string(root.join("xtests/suite.toml"))
        .ok()
        .and_then(|t| crate::config::Manifest::parse(&t).ok())
        .map(|m| m.meta.rss_ceiling_mb)
        .filter(|&mb| mb > 0);
    let peaks = roster.reap_all_checked(ceiling)?;
    let peak_note: Vec<String> = peaks
        .iter()
        .map(|(n, kb)| format!("{n}={} MB", kb / 1024))
        .collect();
    let _ = std::fs::remove_dir_all(&tmp);
    let text = out?;
    let verdict = text
        .lines()
        .rev()
        .find(|l| l.contains("losses="))
        .unwrap_or("(no verdict line)")
        .to_string();
    if !verdict.contains("losses=0") {
        return Err(format!("sweep verdict: {verdict}"));
    }
    // v7.38.19 — the panel's verdict LINE, whether the script exited 0
    // or not.
    //
    // The sweep exits non-zero when it has losses, and this panel
    // EXPECTS losses: collating costs a few percent on shapes that
    // return their rows. So the exit code is not the question — the
    // summary line is.
    //
    // The first version of this asked the question of the whole error
    // TEXT, which carries the script's own output inside it, and the
    // output carries the summary. So a failed run passed, because the
    // words being looked for were sitting in the failure message. One
    // gate run went green on exactly that. The line is extracted from
    // either outcome now, and its ABSENCE is the failure: a script that
    // printed no summary never got far enough to have one.
    let both = match &locale_out {
        Ok(t) => t.clone(),
        Err(e) => e.clone(),
    };
    // v7.38.22 — two panels arrive in one string, so grade them apart.
    //
    // `verdict_line` reads BACKWARDS for the last `cells=` line. With the
    // shipped-default panel appended, that line belongs to the wrong
    // panel, and the locale panel would be graded on someone else's
    // numbers.
    let (locale_text, shipped_text) = match both.split_once("===SHIPPED-DEFAULT===") {
        Some((a, b)) => (a.to_string(), Some(b.to_string())),
        None => (both.clone(), None),
    };
    let Some(locale_verdict) = verdict_line(&locale_text) else {
        return Err(format!(
            "locale-collation panel: no verdict line — it never got far enough \
             to have one: {}",
            locale_text.lines().take(6).collect::<Vec<_>>().join(" / ")
        ));
    };
    // v7.38.23 — 2.0x for the sort half here, where the PG18 panel keeps
    // 3.0x. The two panels ask different questions and the bar follows
    // the question: against PostgreSQL a text sort may legitimately cost
    // a multiple, but a binary against ITSELF under a collation that
    // orders the data exactly as bytes do should cost almost nothing.
    //
    // 3.0x was chosen to catch the v7.38.18 regression that cost
    // twenty-six times, and a bar set for 26x does not stop 2.7x: this
    // release's defect measured 2.89x and 2.91x on the two row-returning
    // cells and passed. Worse, their spreads reached 3.08 and 3.15, so
    // the old bar would have flapped on it rather than caught it.
    //
    // Both sides of 2.0 are measured, one instrument, twelve pairs a
    // cell, each build named by md5:
    //
    //   shipped (f9530767)   2.89x (2.67-3.08)   2.91x (2.84-3.15)   RED
    //   fixed   (fd2e3501)   1.13x               1.25x               green
    //
    // and the fixed build's `sort_worst` across four runs whose own
    // control did not fire reads 1.27x, 1.30x, 1.35x, 1.44x. A run where
    // `control_false_differences` is non-zero is the same binary
    // separating from ITSELF, which this step already refuses to grade;
    // one such run read 1.96x and is not a verdict.
    //
    // Worst trustworthy green 1.44x, bar 2.0x, defect 3.07x. Margin on
    // both sides, which is the whole point of choosing it from data.
    // v7.38.19 — the panel is judged on its COST CLASS, which is the
    // question it says it asks, and not on whether any cell separated.
    //
    // It inherited the sweep's own verdict: any cell outside the noise
    // band is a loss. For SPG-against-PostgreSQL that is right. For a
    // binary against ITSELF under two collations it is not, because
    // collating IS more work than not collating and a few percent is
    // what that costs — measured, the three cells that separate are
    // `narrow, non-indexed key` at 4.5%, `filtered then order` at 3%
    // and `descending` at 1.5%, all of them shapes that RETURN their
    // rows, so the wire dominates and the sort is a sliver of the cell.
    //
    // What the panel exists to catch is the regression that put it
    // here: `WHERE kind = 'click'` costing twenty-six times more under
    // a locale than under `C`, shipped in v7.38.18 and found by hand.
    // Its sort half already says this out loud with a 3.0x ceiling; the
    // shapes half is held to the same bar, and `sort_over_ceiling`
    // stays part of the verdict rather than being folded away.
    if !locale_panel_passes(&locale_verdict) {
        return Err(format!(
            "locale-collation panel: {locale_verdict} — a declared collation \
             changed the cost class against the same binary under `C`"
        ));
    }
    // v7.38.22 — the shipped default REPORTS, it does not judge. Yet.
    //
    // The sixty-four cells compare `C` against `C`; the image now ships
    // `LANG=en_US.utf8`, so that is no longer the configuration a
    // customer runs. This panel is the one that is: SPG under the
    // locale, against PostgreSQL under the same locale.
    //
    // No bar on it in the release that introduces it. The locale panel
    // was introduced the same way, and the reason is the same — a bar
    // set before the distribution is known is a bar that flaps, and a
    // flapping gate teaches the reader to skip the line.
    let shipped_note = shipped_text
        .as_deref()
        .and_then(verdict_line)
        .unwrap_or_else(|| "(no verdict line — see the step's output)".to_string());
    Ok(format!(
        "{verdict}; locale panel {locale_verdict}; shipped-default panel \
         (SPG en_US vs PG18 en_US, reported not judged) {shipped_note}; peak rss: {}",
        peak_note.join(", ")
    ))
}

/// v7.38.19 — what a locale-collation panel verdict must NOT say.
///
/// The sweep's own summary line, wherever it is — in a successful run's
/// output or inside a failed run's error text. `None` means the script
/// never printed one, which is the only shape that cannot be judged.
fn verdict_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .find(|l| l.trim_start().starts_with("cells=") && l.contains("losses="))
        .map(str::to_string)
}

/// Whether a locale-collation panel run says what it has to say.
///
/// Three things, and no more: the sort half stayed inside its cost-class
/// ceiling, the control leg — the same binary against itself — found no
/// differences, and no cell had its verdict withdrawn. A run where the
/// box moved says nothing about collations either way.
///
/// What is deliberately NOT required is `losses=0`. The panel inherited
/// that from the sweep, where it is right: against PostgreSQL, any cell
/// outside the noise band is a loss. Against the SAME BINARY under two
/// collations it is not, because collating is more work than not
/// collating. Measured, the cells that separate are `narrow,
/// non-indexed key` at 4.5 %, `filtered then order` at 3 % and
/// `descending` at 1.5 % — all shapes that RETURN their rows, where the
/// wire dominates and the sort is a sliver.
///
/// What the panel exists to catch is what put it here: `WHERE kind =
/// 'click'` costing twenty-six times more under a locale than under
/// `C`, shipped in v7.38.18 and found by hand. Its sort half says that
/// out loud with a 3.0x ceiling.
fn locale_panel_passes(verdict: &str) -> bool {
    // One LINE, not a haystack. Handed a whole error text — which
    // carries the script's own output, which carries this summary — a
    // contains-check would find every word it is looking for and pass a
    // run that failed. One gate run went green on exactly that. Refusing
    // multi-line input makes the misuse impossible rather than
    // remembered.
    !verdict.contains('\n')
        && verdict.contains("sort_over_ceiling=0")
        && verdict.contains("control_false_differences=0")
        && verdict.contains("withdrawn=0")
}

#[cfg(test)]
mod locale_panel_verdict_tests {
    use super::locale_panel_passes;

    const CLEAN: &str = "cells=16 losses=3 control_false_differences=0 withdrawn=0 sort_worst=2.32x sort_over_ceiling=0";

    #[test]
    fn a_few_percent_on_the_shapes_is_not_a_cost_class_change() {
        assert!(locale_panel_passes(CLEAN));
    }

    #[test]
    fn the_things_it_still_refuses() {
        for bad in [
            // The 26x regression this panel exists for.
            "cells=16 losses=3 control_false_differences=0 withdrawn=0 sort_worst=78.14x sort_over_ceiling=3",
            // The box moved; nothing here is worth reading.
            "cells=16 losses=0 control_false_differences=2 withdrawn=0 sort_worst=1.1x sort_over_ceiling=0",
            // A cell's verdict was withdrawn, for the same reason.
            "cells=16 losses=0 control_false_differences=0 withdrawn=1 sort_worst=1.1x sort_over_ceiling=0",
            // No verdict line at all.
            "(no verdict line)",
            // The leg never answered.
            "locale leg failed: connection refused",
        ] {
            assert!(!locale_panel_passes(bad), "{bad}");
        }
    }

    /// The hole the first version had: a FAILED run's error text carries
    /// the script's own output, and the output carries the summary. Ask
    /// the whole error text and the words being looked for are sitting
    /// right there, so the run passes for having failed loudly enough.
    /// One gate run went green on exactly that.
    ///
    /// The line has to be extracted first, and its absence is the
    /// failure.
    #[test]
    fn the_verdict_is_a_line_not_a_haystack() {
        let failed_but_reported = format!(
            "locale leg failed: `bash scripts/perf-endpoint-sweep.sh` exited exit status: 1:\n\
             load before: 11:11\n{CLEAN}\n"
        );
        assert_eq!(
            super::verdict_line(&failed_but_reported).as_deref(),
            Some(CLEAN),
            "the summary is in there and must be found"
        );
        assert!(super::verdict_line("locale leg failed: connection refused").is_none());
        // And the haystack itself must not be mistaken for a verdict.
        assert!(!locale_panel_passes(&failed_but_reported));
    }
}

/// `ironrules` (S1.3) — the prerelease tier's standing-rule step: the
/// wire smoke below, PLUS the previous release's data directory opened
/// directly by the CURRENT binary and verified row-for-row.
///
/// The fixture is captured by each release tag's own binary: 500 rows
/// across nine types, two indexes, deletes and updates, statement-level
/// WAL (there is no db file; replay IS the open). `expected.txt` holds
/// counts and an md5 over an ordered projection, so a silently thinner
/// replay cannot pass. Older fixtures stay on disk beside it (S3.2).
///
/// v7.38.17 — WHICH fixture is chosen, rather than a hard-coded path.
/// It read `v7.38.15` literally, so every release captured a fixture
/// (the S3.2 protocol) that nothing then opened, and the step's name --
/// "the PREVIOUS release's data directory" -- aged into a lie the day
/// 7.38.16 shipped. Newest fixture wins, current version excluded: a
/// build opening its own fixture is not a cross-version test.
///
/// # Errors
/// Any probe or any fixture assertion failing, named. No usable fixture
/// is an error too: silently having nothing to open is how this step
/// would stop testing anything at all.
/// The newest `xtests/compat-datadirs/vX.Y.Z` that is not this
/// workspace's own version.
/// The oldest and the newest data-directory fixture older than the
/// workspace version.
///
/// v7.38.18 — this returned only the newest, and so the cross-version
/// open only ever exercised a one-release hop. That is the hop least
/// likely to break: it is the same `FILE_VERSION` almost every time.
/// This release moved `FILE_VERSION` 91 -> 92 for the database
/// collation, and the customer this was written for is eleven releases
/// back — the jump the step could not see was exactly the one being
/// promised to them in writing.
fn prior_datadirs(root: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let current = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("read Cargo.toml: {e}"))?
        .lines()
        .find_map(|l| {
            let l = l.trim();
            l.strip_prefix("version")
                .and_then(|r| r.split('"').nth(1))
                .map(str::to_string)
        })
        .ok_or("no workspace version in Cargo.toml")?;
    let dir = root.join("xtests/compat-datadirs");
    let mut best: Option<(Vec<u32>, std::path::PathBuf)> = None;
    let mut worst: Option<(Vec<u32>, std::path::PathBuf)> = None;
    for e in std::fs::read_dir(&dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
    {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(ver) = name.strip_prefix('v') else {
            continue;
        };
        if ver == current || !e.path().join("expected.txt").exists() {
            continue;
        }
        let parts: Vec<u32> = ver.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() != 3 {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| parts > *b) {
            best = Some((parts.clone(), e.path()));
        }
        if worst.as_ref().is_none_or(|(w, _)| parts < *w) {
            worst = Some((parts, e.path()));
        }
    }
    let (Some((_, newest)), Some((_, oldest))) = (best, worst) else {
        return Err(format!(
            "no data-directory fixture older than {current} under {} — the \
             cross-version open has nothing to open",
            dir.display()
        ));
    };
    // One entry when they are the same directory, so the caller opens it
    // once and the report does not claim two hops it did not make.
    if oldest == newest {
        return Ok(vec![newest]);
    }
    Ok(vec![oldest, newest])
}

pub fn ironrules_full(root: &Path, runid: &str) -> Result<String, String> {
    let smoke = ironrule_smoke(root, runid)?;
    let mut opened = Vec::new();
    for fixture in prior_datadirs(root)? {
        opened.push(open_datadir_fixture(root, &fixture, runid)?);
    }
    // Name every fixture that was opened. The old line said "v7.38.15"
    // whatever ran, which is how a report keeps announcing coverage a
    // step no longer has.
    Ok(format!(
        "{smoke}; {} dir direct-open verified",
        opened.join(" + ")
    ))
}

/// Open one released data directory with the binary being released, and
/// check every count and checksum the fixture recorded.
fn open_datadir_fixture(root: &Path, fixture: &Path, runid: &str) -> Result<String, String> {
    // The fixture's own name, so a failure sends the reader to the
    // directory that actually failed. These messages said `v7.38.15`
    // literally, which was the same aging lie as the path they came
    // from — and an error naming the wrong artefact is worse than one
    // naming none.
    let fx = fixture
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-fver-{fx}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("mkdir: {e}"))?;
    // The server mutates its dir; always open a COPY.
    for f in ["audit", "wal", "wal.cluster_id"] {
        std::fs::copy(fixture.join(f), tmp.join(f)).map_err(|e| format!("copy {f}: {e}"))?;
    }
    let bin = root.join("target/release/spg-server");
    let mut roster = Roster::new();
    let port = roster.spawn_server("fver", &bin, &tmp, Duration::from_secs(15))?;
    let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite")?;
    let expected = std::fs::read_to_string(fixture.join("expected.txt"))
        .map_err(|e| format!("expected.txt: {e}"))?;
    for line in expected.lines() {
        let Some((key, want)) = line.trim().split_once(' ') else {
            continue;
        };
        let sql = if key == "checksum" {
            "SELECT md5(string_agg(t, ',' ORDER BY id)) FROM fx_scalars".to_string()
        } else {
            format!("SELECT count(*) FROM {key}")
        };
        let r = conn.simple_query(&sql)?;
        if let Some(e) = r.error {
            return Err(format!("{fx} fixture: {sql}: {e}"));
        }
        let got = r
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default();
        if got != want {
            return Err(format!(
                "{fx} fixture: {key}: want {want}, got {got} — that \
                 release's data did not survive the current binary"
            ));
        }
    }
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(fx)
}

/// `ironrule-smoke` — the fastest wire-level pins of standing rules:
///
/// 1. `wal_path` is really plumbed (r964's hard lesson): after one
///    write, the WAL file at OUR path is non-empty.
/// 2. The pgwire listener answers psql's first packet (SSLRequest).
/// 3. A zero-column result set still carries its rows (the r800 gate
///    relaxation lost them once): `SELECT FROM t` returns one DataRow
///    per row, zero fields each.
///
/// # Errors
/// Any probe failing, with the probe named.
pub fn ironrule_smoke(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        return Err(format!(
            "{} not built — precommit needs the release server once; run `cargo build --release -p spg-server`",
            bin.display()
        ));
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-ironrule"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let port = roster.spawn_server("ironrule", &bin, &tmp, Duration::from_secs(15))?;

    // Probe 2 first — it needs no state.
    let answer = crate::wireclient::ssl_request_answered(port)?;
    if answer != b'S' && answer != b'N' {
        return Err(format!(
            "SSLRequest answered {answer:#x}, expected 'S' or 'N'"
        ));
    }

    let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite")?;
    let run = |c: &mut crate::wireclient::Conn,
               sql: &str|
     -> Result<crate::wireclient::QueryResult, String> {
        let r = c.simple_query(sql)?;
        match &r.error {
            Some(e) => Err(format!("{sql}: {e}")),
            None => Ok(r),
        }
    };
    run(&mut conn, "CREATE TABLE irt (a INT)")?;
    run(&mut conn, "INSERT INTO irt VALUES (1), (2)")?;

    // Probe 3 — zero-column rows.
    let zc = run(&mut conn, "SELECT FROM irt")?;
    if zc.rows.len() != 2 || zc.n_columns != 0 {
        return Err(format!(
            "zero-column SELECT: want 2 rows x 0 cols, got {} rows x {} cols",
            zc.rows.len(),
            zc.n_columns
        ));
    }

    // Probe 1 — the WAL at OUR path grew past its header.
    let wal = tmp.join("wal");
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    if wal_len == 0 {
        return Err(format!(
            "wal_path not plumbed: {} is empty after two writes",
            wal.display()
        ));
    }

    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(format!(
        "ssl answered '{}', wal {} bytes, zero-col rows intact",
        answer as char, wal_len
    ))
}

/// full-tier `perm-matrix` — the whole permutation matrix, whole
/// corpus (no --fast, no sampling). Builds the release server first:
/// the two wire permutations refuse to guess at a stale binary.
///
/// # Errors
/// Build failure or any permutation reporting failures.
pub fn perm_matrix(root: &Path) -> Result<String, String> {
    sh(
        root,
        "cargo build -q --release -p spg-server -p spg-perm-runner",
    )?;
    sh(root, "cargo run -q --release -p spg-perm-runner -- all").map(|out| tail_lines(&out, 10))
}

/// full-tier `oracle-three` — bring up the D13-pinned compose stack,
/// run all three differential legs, and ALWAYS tear the stack down
/// (zombie discipline): the teardown runs whether the legs pass or
/// not, and a teardown failure surfaces even on a green run.
///
/// # Errors
/// Stack startup, any leg's unexplained diff, or teardown failure.
pub fn oracle_three(root: &Path) -> Result<String, String> {
    // OrbStack keeps docker off the default PATH on the runners.
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let path = std::env::var("PATH").unwrap_or_default();
    if !path.split(':').any(|p| p == orb) && std::path::Path::new(orb).exists() {
        // Safety: the suite runner is effectively single-threaded at
        // this point (steps run sequentially).
        unsafe { std::env::set_var("PATH", format!("{path}:{orb}")) };
    }
    sh(root, "cargo build -q --release -p spg-oracle-runner")?;
    sh(
        root,
        "docker compose -f xtests/oracle/docker-compose.yml up -d --wait",
    )?;
    let legs = sh(root, "cargo run -q --release -p spg-oracle-runner -- all");
    let down = sh(
        root,
        "docker compose -f xtests/oracle/docker-compose.yml down -v",
    );
    let summary = legs?;
    down?;
    Ok(tail_lines(&summary, 4))
}

fn tail_lines(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// full-tier `generative` — the S4.2 differ: 10^4 seeded statements,
/// three legs (embedded / simple / extended), zero divergence. The
/// seed derives from the runid's git sha so a red night replays
/// exactly (`spg-gendiff --seed <printed>`).
///
/// # Errors
/// Build failure, or any divergence (drafts land in 15_regressions).
pub fn generative(root: &Path, runid: &str) -> Result<String, String> {
    sh(
        root,
        "cargo build -q --release -p spg-server -p spg-gendiff",
    )?;
    let seed = runid
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(131).wrapping_add(u64::from(b)));
    // 7.38.1 S6.1 (D8) — the live-PG fourth leg rides whenever the
    // oracle container is reachable. 10^4 is the CP judgement; 10^5 is
    // the nightly parameter (SPG_GENDIFF_COUNT overrides).
    //
    // gendiff is a HOST binary dialing the oracle's published port, so
    // the leg is always 127.0.0.1 — `host.docker.internal` is a name
    // only resolvable INSIDE containers (the perf sweep uses it because
    // its psql wrapper runs in one; the first mini full to reach this
    // step aborted on that borrowed detection, 2026-08-19).
    let pg_host = "127.0.0.1";
    let count = std::env::var("SPG_GENDIFF_COUNT").unwrap_or_else(|_| String::from("10000"));
    sh(
        root,
        &format!(
            "SPG_GENDIFF_PG='{pg_host}:25432:bench:bench'              cargo run -q --release -p spg-gendiff -- --seed {seed} --count {count}"
        ),
    )
    .map(|out| tail_lines(&out, 2))
}

/// full-tier `sql2016` — the D16 coverage ledger's machine check: no
/// empty cells, every named corpus path exists, and the uncovered
/// count prints as the ledger that must shrink release over release.
///
/// # Errors
/// Malformed rows, unknown statuses, or a named corpus path that
/// doesn't exist (a moved file must move its ledger row with it).
pub fn sql2016(root: &Path) -> Result<String, String> {
    let path = root.join("xtests/sqllogictest/SQL2016-COVERAGE.tsv");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let corpus = root.join("xtests/sqllogictest/corpus");
    let (mut covered, mut partial, mut uncovered) = (0usize, 0usize, 0usize);
    for (ln, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.len() != 5 || cells.iter().any(|c| c.trim().is_empty()) {
            return Err(format!(
                "SQL2016-COVERAGE.tsv:{}: need 5 non-empty cells",
                ln + 1
            ));
        }
        match cells[3] {
            "covered" => covered += 1,
            "partial" => partial += 1,
            "uncovered" => {
                uncovered += 1;
                if cells[4] != "-" {
                    return Err(format!(
                        "SQL2016-COVERAGE.tsv:{}: uncovered row must carry `-`",
                        ln + 1
                    ));
                }
                continue;
            }
            other => {
                return Err(format!(
                    "SQL2016-COVERAGE.tsv:{}: unknown status {other}",
                    ln + 1
                ));
            }
        }
        if !corpus.join(cells[4]).exists() {
            return Err(format!(
                "SQL2016-COVERAGE.tsv:{}: corpus path {} does not exist",
                ln + 1,
                cells[4]
            ));
        }
    }
    Ok(format!(
        "sql2016 ledger: covered={covered} partial={partial} uncovered={uncovered}"
    ))
}

/// full-tier `pgbench` (7.38 S5.1, D21) — pgbench's builtin tpcb-like
/// against SPGS over the wire, with a same-machine PG18 control leg
/// (the bench container runs both client and control server). The
/// drop-in bar: `pgbench -i` + the run COMPLETE, and the single-client
/// leg finishes with ZERO failed transactions. The contended leg's
/// failure rate prints into the account — the RC concurrent-UPDATE
/// blocking gap is ledgered (MATRIX #20), not hidden by this step.
///
/// # Errors
/// Server/build failure, init failure, or a single-client failure
/// count above zero.
pub fn pgbench(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-pgb"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let port = roster.spawn_server_on(
        "pgbench-leg",
        &bin,
        &tmp,
        Duration::from_secs(20),
        "0.0.0.0",
    )?;
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let spg_uri = format!("postgres://bench:bench@host.docker.internal:{port}/bench");
    let grade = |out: &str| -> (String, String) {
        let pick = |pat: &str| {
            out.lines()
                .find(|l| l.contains(pat))
                .unwrap_or("(missing)")
                .trim()
                .to_string()
        };
        (pick("tps ="), pick("failed transactions"))
    };
    sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -i -s 1 -q '{spg_uri}'"),
    )?;
    let solo = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 1 -T 10 '{spg_uri}'"),
    )?;
    let (solo_tps, solo_failed) = grade(&solo);
    let cont = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 4 -j 2 -T 10 '{spg_uri}'"),
    )?;
    let (cont_tps, cont_failed) = grade(&cont);
    // Control leg: PG18 inside the same container (its own server).
    sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -i -s 1 -q -U bench bench"),
    )?;
    let pg = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 1 -T 10 -U bench bench"),
    )?;
    let (pg_tps, _) = grade(&pg);
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    if !solo_failed.contains("0 (0.000%)") {
        return Err(format!(
            "pgbench single-client leg had failures: {solo_failed}"
        ));
    }
    Ok(format!(
        "tpcb s=1: SPG c1 [{solo_tps}] vs PG18 c1 [{pg_tps}]; SPG c4 [{cont_tps}, {cont_failed} — MATRIX #20]"
    ))
}

/// full-tier `sysbench` (7.38 S5.2, D21) — the MySQL-dialect leg:
/// sysbench oltp_read_write over SPG's mysql wire (zero ignored
/// errors required), a same-machine MySQL control leg via the D13
/// oracle image, and — when the Percona tpcc scripts are present at
/// /tmp/sysbench-tpcc — a tpcc leg too (absence is a loud note, not
/// a silent skip). Needs a native `sysbench` on the runner.
///
/// # Errors
/// Missing sysbench, server failure, or any leg with errors.
pub fn sysbench(root: &Path, runid: &str) -> Result<String, String> {
    let sysbench = ["/opt/homebrew/bin/sysbench", "/usr/local/bin/sysbench"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| (*p).to_string())
        .ok_or("sysbench not installed on this runner (brew install sysbench)")?;
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-sb"));
    let _ = std::fs::remove_dir_all(&tmp);
    // The mysql wire rides an env var, so spawn with it set.
    let mut roster = Roster::new();
    let my_port = 25459; // one below the suite pg range; probed by bind
    if std::net::TcpListener::bind(("127.0.0.1", my_port)).is_err() {
        return Err("port 25459 (mysql-wire leg) is taken — janitor time".into());
    }
    let _pg = roster.spawn_server_env(
        "sysbench-leg",
        &bin,
        &tmp,
        Duration::from_secs(20),
        "127.0.0.1",
        &[("SPG_MYSQLWIRE_ADDR", "127.0.0.1:25459")],
    )?;
    let uri = format!(
        "--mysql-host=127.0.0.1 --mysql-port={my_port} --mysql-user=bench --mysql-password=bench --mysql-db=bench"
    );
    sh(
        root,
        &format!("{sysbench} oltp_read_write {uri} --tables=2 --table-size=1000 prepare"),
    )?;
    let run = sh(
        root,
        &format!(
            "{sysbench} oltp_read_write {uri} --tables=2 --table-size=1000 --threads=1 --time=10 run"
        ),
    )?;
    let pick = |out: &str, pat: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(pat))
            .unwrap_or("(missing)")
            .trim()
            .to_string()
    };
    let spg_tx = pick(&run, "transactions:");
    let spg_err = pick(&run, "ignored errors:");
    if !spg_err.contains("0      (0.00 per sec.)")
        && !spg_err.contains("ignored errors:                      0")
    {
        return Err(format!("sysbench SPG leg had errors: {spg_err}"));
    }
    // tpcc leg — the Percona scripts self-fetch to a pinned commit in
    // the user cache (7.38.1 S1.4, D10②): no /tmp inheritance, no
    // floating upstream. Offline runners without the cache get a loud
    // note, never a silent skip.
    const TPCC_PIN: &str = "f110afa8023c7924b1ba00177232a9090624acb5";
    let tpcc_dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/spg-suite/sysbench-tpcc"))
        .map_err(|_| "no $HOME")?;
    if !tpcc_dir.join("tpcc.lua").exists() {
        let _ = sh(
            root,
            &format!(
                "git clone -q https://github.com/Percona-Lab/sysbench-tpcc {} && git -C {} checkout -q {TPCC_PIN}",
                tpcc_dir.display(),
                tpcc_dir.display()
            ),
        );
    } else {
        // Present but drifted? Pin it back — the corpus is a contract.
        let _ = sh(
            root,
            &format!("git -C {} checkout -q {TPCC_PIN}", tpcc_dir.display()),
        );
    }
    let tpcc_note = if tpcc_dir.join("tpcc.lua").exists() {
        sh(
            root,
            &format!(
                "cd {} && {sysbench} ./tpcc.lua {uri} --tables=1 --scale=1 --use_fk=0 prepare",
                tpcc_dir.display()
            ),
        )?;
        let t = sh(
            root,
            &format!(
                "cd {} && {sysbench} ./tpcc.lua {uri} --tables=1 --scale=1 --use_fk=0 --threads=1 --time=10 run",
                tpcc_dir.display()
            ),
        )?;
        format!("; tpcc [{}]", pick(&t, "transactions:"))
    } else {
        format!(
            "; tpcc UNAVAILABLE (clone failed and no cache at {})",
            tpcc_dir.display()
        )
    };
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    // Control leg — the D13 mysql oracle image, same client, same
    // shape. Best-effort: a control that can't start is a note, not
    // a red (the SPG leg above is the gate).
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let control = (|| -> Result<String, String> {
        sh(
            root,
            &format!("{docker} compose -f xtests/oracle/docker-compose.yml up -d --wait mysql"),
        )?;
        let curi = "--mysql-host=127.0.0.1 --mysql-port=15433 --mysql-user=root --mysql-password=testpass --mysql-db=testdb";
        sh(
            root,
            &format!("{sysbench} oltp_read_write {curi} --tables=2 --table-size=1000 prepare"),
        )?;
        let r = sh(
            root,
            &format!(
                "{sysbench} oltp_read_write {curi} --tables=2 --table-size=1000 --threads=1 --time=10 run"
            ),
        );
        let _ = sh(
            root,
            &format!("{docker} compose -f xtests/oracle/docker-compose.yml down -v"),
        );
        r.map(|out| pick(&out, "transactions:"))
    })();
    let control_note = match control {
        Ok(tx) => format!("; MySQL control [{tx}]"),
        Err(e) => format!(
            "; MySQL control UNAVAILABLE ({})",
            e.lines().next().unwrap_or("")
        ),
    };
    Ok(format!(
        "oltp_read_write SPG [{spg_tx}]{tpcc_note}{control_note}"
    ))
}

/// full-tier `pgdump-roundtrip` (7.38.1 S5.2, D6) — the official
/// PG18 pg_dump runs against a live SPG carrying the rich shape set,
/// must EXIT 0, and the dump must restore into a FRESH SPG and into a
/// FRESH PG18 database with the same row counts on a canary query.
/// pg_dump comes from the oracle container (mini) or the host
/// toolchain (local), same detection the sweep uses for psql.
///
/// # Errors
/// pg_dump non-zero, any restore error on the SPG leg, or a count
/// mismatch across the three sides.
pub fn pgdump_roundtrip(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let wrapper = Path::new(&home).join("spgbench/bin/psql");
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let (psql, pg_dump, host, bind) = if wrapper.exists() {
        (
            wrapper.display().to_string(),
            format!("{docker} exec spg-bench-postgres pg_dump"),
            "host.docker.internal",
            "0.0.0.0",
        )
    } else {
        (
            "psql".to_string(),
            "pg_dump".to_string(),
            "127.0.0.1",
            "127.0.0.1",
        )
    };
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-pgdumprt"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let src = roster.spawn_server_on(
        "dump-src",
        &bin,
        &tmp.join("src"),
        Duration::from_secs(30),
        bind,
    )?;
    let dst = roster.spawn_server_on(
        "dump-dst",
        &bin,
        &tmp.join("dst"),
        Duration::from_secs(30),
        bind,
    )?;
    let src_uri = format!("postgres://bench:bench@{host}:{src}/bench");
    let dst_uri = format!("postgres://bench:bench@{host}:{dst}/bench");
    const RICH: &str = "CREATE TABLE rich1 (id BIGSERIAL PRIMARY KEY, tag TEXT[] DEFAULT '{}', \
         amt NUMERIC(12,3), payload JSONB, blob BYTEA, flag BOOLEAN DEFAULT false, \
         created TIMESTAMPTZ DEFAULT now()); \
         CREATE TABLE rich2 (id INT PRIMARY KEY, r1 BIGINT REFERENCES rich1(id) ON DELETE CASCADE, \
         uq TEXT UNIQUE, CHECK (id > 0)); \
         CREATE TYPE addr AS (street TEXT, zip INT); \
         CREATE TABLE rich3 (id INT PRIMARY KEY, home addr, mood_col TEXT); \
         CREATE MATERIALIZED VIEW mv1 AS SELECT count(*) AS n FROM rich1; \
         CREATE TABLE part_parent (id INT, ts DATE) PARTITION BY RANGE (ts); \
         CREATE TABLE part_a PARTITION OF part_parent FOR VALUES FROM ('2026-01-01') TO ('2026-06-01'); \
         CREATE INDEX rich1_gin ON rich1 USING gin (payload); \
         INSERT INTO rich1 (tag, amt, payload, blob) VALUES ('{a,b}', 12.345, '{\"k\":1}', '\\xdeadbeef'); \
         INSERT INTO rich2 VALUES (1, 1, 'x'); \
         INSERT INTO rich3 VALUES (1, ROW('main st', 12345), 'ok'); \
         INSERT INTO part_parent VALUES (1, '2026-02-01');";
    const CANARY: &str = "SELECT (SELECT count(*) FROM rich1) || '|' || \
         (SELECT count(*) FROM rich2) || '|' || (SELECT count(*) FROM rich3) || '|' || \
         (SELECT count(*) FROM part_parent) || '|' || (SELECT (home).zip FROM rich3)";
    let schema_file = tmp.join("rich-schema.sql");
    std::fs::write(&schema_file, RICH).map_err(|e| format!("write schema: {e}"))?;
    sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q '{src_uri}' -f - < {}",
            schema_file.display()
        ),
    )?;
    let dump_file = tmp.join("rich-dump.sql");
    sh(
        root,
        &format!("{pg_dump} '{src_uri}' > {}", dump_file.display()),
    )
    .map_err(|e| format!("pg_dump must exit 0 against SPG: {e}"))?;
    // Leg 1 — fresh SPG. Any ERROR line is a red.
    let restore = sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q '{dst_uri}' -f - < {} 2>&1 | grep -c ERROR || true",
            dump_file.display()
        ),
    )?;
    if restore.trim() != "0" {
        return Err(format!(
            "SPG restore leg had {} error line(s)",
            restore.trim()
        ));
    }
    let src_counts = sh(
        root,
        &format!("{psql} --no-psqlrc -X -q -tA '{src_uri}' -c \"{CANARY}\""),
    )?;
    let dst_counts = sh(
        root,
        &format!("{psql} --no-psqlrc -X -q -tA '{dst_uri}' -c \"{CANARY}\""),
    )?;
    if src_counts.trim() != dst_counts.trim() {
        return Err(format!(
            "SPG roundtrip counts diverge: src={} dst={}",
            src_counts.trim(),
            dst_counts.trim()
        ));
    }
    // Leg 2 — fresh PG18 in the oracle container (skipped, loudly,
    // when no oracle container is reachable — the LOCAL box drives
    // its docker PG through the same 25432 bench container).
    let pg_admin = format!("postgres://bench:bench@{host}:25432/postgres");
    let pg_rt = format!("postgres://bench:bench@{host}:25432/spgdumprt");
    let pg_leg = sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q -tA '{pg_admin}' -c 'DROP DATABASE IF EXISTS spgdumprt' \
             -c 'CREATE DATABASE spgdumprt' && \
             {psql} --no-psqlrc -X -q '{pg_rt}' -f - < {} >/dev/null 2>&1; \
             {psql} --no-psqlrc -X -q -tA '{pg_rt}' -c \"{CANARY}\"",
            dump_file.display()
        ),
    );
    let verdict = match pg_leg {
        Ok(pg_counts) if pg_counts.trim() == src_counts.trim() => {
            format!("three-way OK counts={}", src_counts.trim())
        }
        Ok(pg_counts) => {
            return Err(format!(
                "PG18 leg counts diverge: src={} pg={}",
                src_counts.trim(),
                pg_counts.trim()
            ));
        }
        Err(e) => return Err(format!("PG18 leg failed: {e}")),
    };
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(verdict)
}

#[cfg(test)]
mod datadir_choice_tests {
    use std::path::Path;

    /// v7.38.17 — the cross-version open picks the newest fixture that
    /// is NOT this build's own version.
    ///
    /// It used to be a hard-coded `v7.38.15`, so every release captured
    /// a fixture the S3.2 protocol asked for and nothing ever opened,
    /// and the step's summary line announced `v7.38.15` whatever it had
    /// actually done. The assertion here is the RULE, not a literal, so
    /// it keeps meaning something after the next release.
    #[test]
    fn newest_prior_fixture_excludes_the_current_version() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();
        let current = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("Cargo.toml")
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("version")
                    .and_then(|r| r.split('"').nth(1))
                    .map(str::to_string)
            })
            .expect("workspace version");

        let chosen = super::prior_datadirs(&root).expect("a prior fixture must exist");
        let last = chosen.last().expect("at least one fixture");
        let name = last.file_name().and_then(|s| s.to_str()).unwrap();

        for c in &chosen {
            assert_ne!(
                c.file_name().and_then(|s| s.to_str()).unwrap(),
                format!("v{current}"),
                "a build opening its own fixture is not a cross-version test"
            );
        }

        // And it must be the newest of the ones that qualify, compared
        // numerically: `v7.38.9` sorts after `v7.38.15` as a string, and
        // a lexical pick would quietly test an ancient directory.
        let parts = |n: &str| -> Vec<u32> {
            n.trim_start_matches('v')
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect()
        };
        let chosen_v = parts(name);
        let oldest_name = chosen
            .first()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap()
            .to_string();
        let oldest_v = parts(&oldest_name);
        for e in std::fs::read_dir(root.join("xtests/compat-datadirs"))
            .expect("fixture dir")
            .filter_map(Result::ok)
        {
            let other = e.file_name().to_string_lossy().to_string();
            if other == format!("v{current}") || !e.path().join("expected.txt").exists() {
                continue;
            }
            let v = parts(&other);
            if v.len() == 3 {
                assert!(
                    v <= chosen_v,
                    "{other} is newer than the chosen {name} — the pick is not the newest"
                );
                assert!(
                    v >= oldest_v,
                    "{other} is older than the chosen {oldest_name} — the pick is not the oldest"
                );
            }
        }
    }

    /// v7.38.18 — the step used to open ONE fixture, the newest, which
    /// is the hop least likely to break: it is almost always the same
    /// `FILE_VERSION` on both sides. The far hop is the one a customer
    /// several releases back actually makes, and it was uncovered.
    #[test]
    fn cross_version_open_covers_both_ends() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();
        let chosen = super::prior_datadirs(&root).expect("a prior fixture must exist");
        let names: Vec<String> = chosen
            .iter()
            .map(|p| p.file_name().and_then(|s| s.to_str()).unwrap().to_string())
            .collect();
        let parts = |n: &str| -> Vec<u32> {
            n.trim_start_matches('v')
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect()
        };
        // Two ends, unless the corpus holds exactly one directory.
        let dirs = std::fs::read_dir(root.join("xtests/compat-datadirs"))
            .expect("fixture dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().join("expected.txt").exists())
            .count();
        if dirs > 1 {
            assert_eq!(
                names.len(),
                2,
                "expected the oldest and the newest, got {names:?}"
            );
            assert!(
                parts(&names[0]) < parts(&names[1]),
                "the oldest must come first: {names:?}"
            );
        } else {
            assert_eq!(names.len(), 1);
        }
    }
}
