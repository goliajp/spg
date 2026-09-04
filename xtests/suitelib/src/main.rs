//! suite-run — the v7.38 tier orchestrator (S0.1 skeleton).
//!
//! Design fixed in `.claude/testsuite/` (D1: bash entry stays thin,
//! this binary owns orchestration). At S0.1 it knows its usage and its
//! layers; S0.3 teaches it `xtests/suite.toml`, S0.10 wires the
//! precommit steps.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") | None => {
            print!("{}", usage());
        }
        // S1.3 — run ONE internal step by name, for debugging and for
        // negative controls that must red a single step in isolation.
        Some("step") => {
            let Some(name) = args.get(1) else {
                eprintln!("suite-run step <name>");
                std::process::exit(2);
            };
            let root = std::path::Path::new(".");
            let graph = suitelib::crategraph::CrateGraph::generate(root).expect("graph");
            let runid = "step-debug".to_string();
            let out = match name.as_str() {
                "clippy-changed" => suitelib::steps::clippy_changed(root, &graph),
                "unit-changed" => suitelib::steps::unit_changed(root, &graph),
                "pins-current" => suitelib::steps::pins_current(root),
                "ironrule-smoke" => suitelib::steps::ironrule_smoke(root, &runid),
                "ironrules" => suitelib::steps::ironrules_full(root, &runid),
                "perf-sweep" => suitelib::steps::perf_sweep(root, &runid),
                "perm-matrix" => suitelib::steps::perm_matrix(root),
                "pgdump-roundtrip" => suitelib::steps::pgdump_roundtrip(root, &runid),
                "oracle-three" => suitelib::steps::oracle_three(root),
                "sql2016" => suitelib::steps::sql2016(root),
                "pgbench" => suitelib::steps::pgbench(root, &runid),
                "sysbench" => suitelib::steps::sysbench(root, &runid),
                // v7.38.22 — these three were reachable only from inside a
                // tier run, which is the one place `step` exists to avoid.
                //
                // Its own comment says it is "for debugging and for
                // negative controls that must red a single step in
                // isolation", and three of the ten full-tier steps could
                // not be run that way. It cost a full-tier run to find
                // out: `deep-tier` failed, everything after it skipped,
                // and `isolation` and `generative` could not be reached
                // any other way.
                "isolation" => {
                    suitelib::isolib::run_all(root, std::path::Path::new("xtests/isolation"), false)
                }
                "generative" => suitelib::steps::generative(root, &runid),
                "doc-corpus" => std::process::Command::new("sh")
                    .arg("-c")
                    .arg("cargo run -q --release -p sqllogictest -- --docs")
                    .current_dir(root)
                    .status()
                    .map_err(|e| format!("spawn docs corpus: {e}"))
                    .and_then(|st| {
                        if st.success() {
                            Ok("docs corpus green".to_string())
                        } else {
                            Err(format!("docs corpus exited {st}"))
                        }
                    }),
                other => Err(format!("unknown internal step {other}")),
            };
            match out {
                Ok(note) => println!("ok: {note}"),
                Err(e) => {
                    eprintln!("FAIL: {e}");
                    std::process::exit(1);
                }
            }
        }
        // S4.1 — the isolation battery: `suite-run iso [--bless]`.
        Some("iso") => {
            let bless = args.get(1).map(String::as_str) == Some("--bless");
            let root = std::path::Path::new(".");
            match suitelib::isolib::run_all(root, std::path::Path::new("xtests/isolation"), bless) {
                Ok(note) => println!("ok: {note}"),
                Err(e) => {
                    eprintln!("FAIL: {e}");
                    std::process::exit(1);
                }
            }
        }
        // S0.8 — regenerate the crate graph and stamp it.
        Some("gen-crate-graph") => {
            let root = std::path::Path::new(".");
            match suitelib::crategraph::CrateGraph::generate(root) {
                Ok(g) => {
                    if let Err(e) = std::fs::write(suitelib::crategraph::GRAPH_PATH, g.to_toml()) {
                        eprintln!("suite-run: write graph: {e}");
                        std::process::exit(2);
                    }
                    println!(
                        "crate graph: {} crates, hash {} -> {}",
                        g.deps.len(),
                        g.hash,
                        suitelib::crategraph::GRAPH_PATH
                    );
                }
                Err(e) => {
                    eprintln!("suite-run: {e}");
                    std::process::exit(2);
                }
            }
        }
        Some(tier @ ("precommit" | "prerelease" | "full")) => {
            // S0.3 — parse the manifest and show the tier's step list.
            // Execution arrives with S0.10.
            let manifest_path = "xtests/suite.toml";
            let text = match std::fs::read_to_string(manifest_path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("suite-run: {manifest_path}: {e}");
                    std::process::exit(2);
                }
            };
            let m = match suitelib::config::Manifest::parse(&text) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("suite-run: {e}");
                    std::process::exit(2);
                }
            };
            // S0.8 — staleness guard: a tier must not run against a
            // crate graph older than the manifests (D4).
            let root = std::path::Path::new(".");
            match (
                suitelib::crategraph::CrateGraph::stored_hash(root),
                suitelib::crategraph::CrateGraph::generate(root),
            ) {
                (Ok(stored), Ok(live)) if stored == live.hash => {}
                (Err(e), _) => {
                    eprintln!("suite-run: {e}");
                    std::process::exit(2);
                }
                (Ok(_), Ok(_)) => {
                    eprintln!(
                        "suite-run: crate graph is STALE (a Cargo.toml changed) — \
                         run `suite-run gen-crate-graph` and commit the result"
                    );
                    std::process::exit(2);
                }
                (_, Err(e)) => {
                    eprintln!("suite-run: {e}");
                    std::process::exit(2);
                }
            }
            // S0.10 — execute. Steps run serially (D23); a failure
            // skips the rest (their absence is visible in the report),
            // budgets hard-fail only in precommit (D25).
            let graph = suitelib::crategraph::CrateGraph::generate(root).expect("graph");
            let stamp = run_cmd("date -u +%Y%m%d%H%M%S");
            let sha = run_cmd("git rev-parse --short=7 HEAD");
            let runid = suitelib::reportlib::runid(stamp.trim(), sha.trim());
            let mut ledger = suitelib::reportlib::Ledger::new(tier, &runid);
            // D26 — budget banding: a base/engine-level change honestly
            // costs an engine rebuild; judging it by the cement-level
            // budget makes every deep commit a false red (A19).
            // A24 — cost, not count: one heavy crate is a heavy
            // rebuild all by itself.
            // A24 addendum (r1065): suitelib joined the heavy list on
            // evidence — its test binary (proclib+wireclient+dev deps)
            // measured 48-74 s cold on the local runner, biting the
            // 35 s unit budget three commits running.
            /// v7.38.14 — the ceiling a banded budget may reach. The whole
            /// workspace's unit tests measured 441.6 s on the local runner
            /// for a no-op comment change to `spg-storage`; this leaves
            /// headroom over that and refuses to grow past it.
            const BUDGET_CAP_S: u64 = 480;
            const HEAVY: [&str; 5] = [
                "spg-engine",
                "spg-server",
                "spg-storage",
                "spg-sql",
                "suitelib",
            ];
            // v7.38.14 — band by the AFFECTED COUNT, and apply the cap the
            // message has always claimed.
            //
            // This printed "cap 480 s" while the code multiplied by a flat 8
            // and capped nothing. The two disagreed, and the gap is not
            // academic: a change to `spg-storage` rebuilds and tests all
            // TWENTY-ONE crates, was judged against 35x8x1.2 = 336 s, and
            // measured 390 s. Its own baseline -- HEAD plus a COMMENT on
            // `spg-storage`, no behaviour change at all -- measured 441.6 s.
            // So the budget rejected commits for being what a base-crate
            // change costs, which is the false red the D26 banding exists to
            // prevent, reintroduced by under-counting the band.
            //
            // Evidence-adjusted the way r1065 adjusted the HEAVY list: a
            // number that was measured, with the measurement written down.
            // The cap is what keeps this a budget rather than a blank
            // cheque -- 441.6 s is the whole workspace, so 480 s leaves
            // headroom over the worst case and nothing above it.
            let affected = suitelib::steps::changed_crates(root, &graph)
                .map(|c| {
                    let a = graph.affected(&c);
                    let heavy = a.iter().any(|x| HEAVY.contains(&x.as_str()));
                    if a.len() >= 8 || heavy {
                        a.len().max(8) as u64
                    } else {
                        1
                    }
                })
                .unwrap_or(1);
            let band = if tier == "precommit" { affected } else { 1u64 };
            ledger.band = band;
            if band > 1 {
                println!(
                    "budget band: BASE ({band} crates affected; clippy/unit x{band}x1.2, cap 480 s)"
                );
            }
            println!("tier {tier} — {} steps, run {runid}", m.tier(tier).len());
            // S4.4 — /tmp leak assertion (the S2.4 half deferred here):
            // full tier snapshots /tmp/spg-* before its steps and any
            // NEW survivor at the end is a red, not a shrug.
            let tmp_before: std::collections::BTreeSet<String> = if tier == "full" {
                tmp_spg_entries()
            } else {
                std::collections::BTreeSet::new()
            };
            // v7.38.18 — precommit compiles ONCE, before anything is
            // timed, and that compile is in neither a step budget nor the
            // tier total.
            //
            // The budgets claim to measure the change. They were measuring
            // the build cache. `slt-smoke` is `cargo run -q -p
            // sqllogictest`: the corpus it runs is 1.5-2.0 s for all 413
            // cases, and the step took 0.8 s or 19.7 s depending on whether
            // some earlier command had already built the workspace in
            // debug. At a 15 s budget the split ran straight through the
            // middle, and a HARD gate whose colour a warm cache decides is
            // not a gate.
            //
            // It failed the v7.38.18 release commit, which touched
            // CHANGELOG.md and nothing else — its own affected-crate steps
            // correctly skipped, while the rebuild left by the three
            // commits before it landed on this one. `band` is computed from
            // the diff; the cost comes from the cache, and those are
            // different questions.
            //
            // This is v7.38.14's finding one level down. That release
            // banded the TIER cap after a no-op change could not clear it —
            // "a cap that a no-op cannot clear is not measuring the change,
            // it is measuring the workspace" — and left the per-step
            // budgets flat, measuring exactly that.
            //
            // The time is printed rather than dropped: a total that cannot
            // show what it excluded is a total that overstates.
            if tier == "precommit" {
                // v7.38.19 — the second line is the one that was missing,
                // and its absence cost four release commits.
                //
                // `--all-targets` does not build a lib's UNIT-test
                // harness: that is the crate compiled with `--cfg test`,
                // a different artifact from the lib, the bins, the
                // integration tests and the benches. So `unit-affected`
                // was compiling twelve crates before it could run a
                // single test, inside a hard 480 s budget, and reported
                // 487 s, 658 s and 723 s on three tries while every step
                // in it was green. The same command with the harnesses
                // already built takes 132 s.
                //
                // Which is exactly what the note above says prepare is
                // for: a budget that a compile can blow is measuring the
                // workspace, not the change.
                // v7.38.22 — and a THIRD, over the same crate selection
                // the step will use.
                //
                // The two above warm the workspace. `unit-affected` then
                // runs `cargo test -p A -p B …`, and Cargo resolves
                // features over the SELECTED members — a subset gets a
                // different feature set for the shared dependencies, so
                // none of what was just built could be reused and the
                // step rebuilt from scratch inside a hard 480 s budget.
                //
                // Same step, same band, five runs from the reports:
                // 2289.9 s, 167.8 s, 1585.8 s, 8.9 s, 505.2 s. That is a
                // 257x spread, and the short readings are the ones where
                // a previous run happened to leave the subset warm.
                // v7.39.12 — prepare builds the SAME selection the step
                // runs, which is workspace-wide.
                //
                // It built `-p A -p B … --lib --bins` over the affected
                // closure while `unit_changed` (also v7.39.12) moved to
                // one workspace-wide build. Two selections, so cargo
                // resolved features twice and rebuilt between them: the
                // ledger's own total read 871 s where the steps summed
                // to 593 s, and those 278 s were this prepare building
                // artefacts the step then could not use.
                //
                // One selection, named in one place. `affected_selection`
                // still decides WHICH harnesses run; it no longer
                // decides what gets built.
                // v7.39.12 — the member set the steps use, not a
                // second one. See `steps::precommit_selection`: cargo
                // resolves features over the selected members, so a
                // prepare that names a different set builds artefacts
                // the step cannot use and rebuilds on the way in.
                //
                // The ledger caught the last instance of that without
                // being asked: its own total read 871 s on a run whose
                // six steps summed to 593 s, and the ledger starts
                // before the prepare while the step timer starts after
                // it.
                let subset_args: Vec<String> = suitelib::steps::precommit_selection(root, &graph)
                    .ok()
                    .flatten()
                    .map(|flags| {
                        let mut v = vec!["test".to_string(), "-q".to_string()];
                        v.extend(flags.split_whitespace().map(str::to_string));
                        // v7.39.12 — `--tests` too, so the release's e2e
                        // finds them built.
                        //
                        // Same members is not enough: the release builds
                        // the integration test targets and this tier did
                        // not, so they were cold at release time, every
                        // time. Measured back to back on the testbed —
                        // `gate.sh e2e` right after a precommit, then
                        // again with nothing changed between:
                        //
                        //     1,937 s   then   230 s
                        //
                        // A commit pays that once and the release pays
                        // nothing for it, which is the trade this tier
                        // is for: the commit loop is where waiting is
                        // cheap.
                        v.extend(
                            ["--lib", "--bins", "--tests", "--locked", "--no-run"]
                                .map(str::to_string),
                        );
                        v
                    })
                    .unwrap_or_default();
                let subset_ref: Vec<&str> = subset_args.iter().map(String::as_str).collect();
                // v7.39.11 — prepare builds the one thing the tier
                // actually runs, and nothing else.
                //
                // It used to run three cargo invocations. The first,
                // `build --workspace --all-targets`, compiles every
                // integration-test binary in the workspace — the e2e
                // suites, the perf_gate targets, the xtests — none of
                // which this tier runs; it was the dominant cost of the
                // whole hook. The second warmed the WORKSPACE-resolved
                // unit harnesses, which the step cannot reuse for the
                // reason written above: Cargo resolves features over the
                // selected members, so the subset gets different
                // artifacts. Only the third built what `unit-affected`
                // then runs.
                //
                // Measured on this host, same tier: `unit-affected`
                // 3136.9 s and `clippy-affected` 387.3 s at band 12, on
                // a run whose every step passed. A pre-commit hook that
                // costs fifty minutes is one that gets bypassed, and it
                // was — 7.39.9 and 7.39.10 were both committed with
                // `--no-verify`.
                //
                // `clippy-affected` builds its own artifacts (clippy
                // compiles with a different driver and shares nothing
                // with rustc's), and `slt-smoke` builds its own binary,
                // so neither lost a warm-up it was using.
                let mut prep: Vec<(&[&str], &str)> = Vec::new();
                if !subset_ref.is_empty() {
                    prep.push((
                        subset_ref.as_slice(),
                        "cargo test <shared members> --no-run --lib --bins --tests",
                    ));
                }
                for (args, label) in prep {
                    let t_prep = std::time::Instant::now();
                    // v7.38.25 — `--lib` is an error, not a no-op, when no
                    // selected package has a lib target, and the affected
                    // closure is often one bins-only crate.
                    // `steps::unit_changed` has retried without it since
                    // v7.38.2; this prepare, which runs first and exits the
                    // tier on any failure, did not -- so a commit touching
                    // only `spg-dogfood-replay` could not reach the step
                    // that knows how to cope. Same defect, same repository,
                    // fixed in one of the two places that has it.
                    let run = |a: &[&str]| {
                        std::process::Command::new("cargo")
                            .args(a)
                            .current_dir(root)
                            .output()
                    };
                    let mut out = run(args);
                    let no_lib = |o: &std::process::Output| {
                        String::from_utf8_lossy(&o.stderr).contains("no library targets")
                    };
                    let mut retried = "";
                    if matches!(&out, Ok(o) if !o.status.success() && no_lib(o)) {
                        let without: Vec<&str> =
                            args.iter().copied().filter(|a| *a != "--lib").collect();
                        out = run(&without);
                        retried = " (retried without --lib)";
                    }
                    let took = t_prep.elapsed();
                    match out {
                        Ok(o) if o.status.success() => println!(
                            "prepare: {label}{retried} in {took:?} \
                             (outside every budget and the tier total)"
                        ),
                        Ok(o) => {
                            eprint!("{}", String::from_utf8_lossy(&o.stderr));
                            eprintln!(
                                "prepare: {label} failed ({}) — nothing below would mean anything",
                                o.status
                            );
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("prepare: {label} could not run: {e}");
                            std::process::exit(1);
                        }
                    }
                }
            }
            let mut failed: Option<String> = None;
            let t_total = std::time::Instant::now();
            let tier_steps = m.tier(tier);
            let mut idx = 0usize;
            while idx < tier_steps.len() {
                let s = tier_steps[idx];
                if failed.is_some() {
                    ledger.record_skip(&s.name);
                    println!("  SKIP  {:<16} (earlier step failed)", s.name);
                    idx += 1;
                    continue;
                }
                // S4.6 (D23 解禁) — adjacent EXTERNAL steps sharing a
                // `group` run concurrently on threads; the ledger
                // records them in FILE order whatever the finish
                // order, so reports stay byte-diffable run to run.
                if s.group.is_some() {
                    let mut batch = vec![s];
                    while idx + batch.len() < tier_steps.len()
                        && tier_steps[idx + batch.len()].group == s.group
                    {
                        batch.push(tier_steps[idx + batch.len()]);
                    }
                    if batch.len() > 1 {
                        if let Some(bad) = batch.iter().find(|b| b.implementation != "external") {
                            eprintln!(
                                "suite-run: step {} is internal — parallel groups take external steps only",
                                bad.name
                            );
                            std::process::exit(2);
                        }
                        println!(
                            "  ∥ group {:?}: {:?}",
                            s.group.as_deref().unwrap_or(""),
                            batch.iter().map(|b| b.name.as_str()).collect::<Vec<_>>()
                        );
                        let results: Vec<(
                            String,
                            Option<std::time::Duration>,
                            std::time::Duration,
                            bool,
                        )> = std::thread::scope(|scope| {
                            let handles: Vec<_> = batch
                                .iter()
                                .map(|b| {
                                    let cmd = b.cmd.clone().expect("checked by parse");
                                    let name = b.name.clone();
                                    let budget = b.budget_s.map(std::time::Duration::from_secs);
                                    scope.spawn(move || {
                                        let t0 = std::time::Instant::now();
                                        let ok = std::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(&cmd)
                                            .status()
                                            .map(|st| st.success())
                                            .unwrap_or(false);
                                        (name, budget, t0.elapsed(), ok)
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|h| h.join().expect("group thread"))
                                .collect()
                        });
                        for (name, budget, dur, ok) in &results {
                            ledger.record_result(name, *budget, *dur, *ok);
                            println!(
                                "  {}  {:<16} ({:.1}s)",
                                if *ok { "ok  " } else { "FAIL" },
                                name,
                                dur.as_secs_f64()
                            );
                            if !ok && failed.is_none() {
                                failed = Some(format!("step {name} failed"));
                            }
                        }
                        if results.iter().any(|(n, ..)| n == "biz")
                            && std::env::var("SUITE_KEEP_REPORTS").is_err()
                        {
                            let _ = std::process::Command::new("git")
                                .args([
                                    "checkout",
                                    "--",
                                    "xtests/sqllogictest/report.json",
                                    "xtests/sqllogictest/report.md",
                                    "xtests/data_compat/report.md",
                                ])
                                .status();
                        }
                        idx += batch.len();
                        continue;
                    }
                }
                // A23 — the BASE band covers every affected-closure
                // step (an engine change widens clippy exactly as it
                // widens unit), with 1.2x headroom on the banded value.
                let budget_secs = s.budget_s.map(|b| {
                    if band > 1
                        && matches!(
                            s.name.as_str(),
                            "unit-changed" | "clippy-changed" | "pins-current" | "slt-smoke"
                        )
                    {
                        (b * band * 12 / 10).min(BUDGET_CAP_S)
                    } else {
                        b
                    }
                });
                let budget = budget_secs.map(std::time::Duration::from_secs);
                let name = s.name.clone();
                let outcome = ledger.step(&name, budget, || match s.implementation.as_str() {
                    "external" => {
                        let cmd = s.cmd.clone().expect("checked by parse");
                        let st = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&cmd)
                            .status()
                            .map_err(|e| format!("spawn: {e}"))?;
                        if st.success() {
                            Ok(String::new())
                        } else {
                            Err(format!("`{cmd}` exited {st}"))
                        }
                    }
                    _ => match s.name.as_str() {
                        "clippy-changed" => suitelib::steps::clippy_changed(root, &graph),
                        "unit-changed" => suitelib::steps::unit_changed(root, &graph),
                        "pins-current" => suitelib::steps::pins_current(root),
                        "ironrule-smoke" => suitelib::steps::ironrule_smoke(root, &runid),
                        // S1.3 — smoke plus the previous release's data
                        // directory opened by the current binary.
                        "ironrules" => suitelib::steps::ironrules_full(root, &runid),
                        "perf-sweep" => suitelib::steps::perf_sweep(root, &runid),
                        // full tier (CP3) — the two 元机制 carriers.
                        "perm-matrix" => suitelib::steps::perm_matrix(root),
                        "pgdump-roundtrip" => suitelib::steps::pgdump_roundtrip(root, &runid),
                        "oracle-three" => suitelib::steps::oracle_three(root),
                        // S4.1 — isolation battery.
                        "isolation" => suitelib::isolib::run_all(
                            root,
                            std::path::Path::new("xtests/isolation"),
                            false,
                        ),
                        // S4.2 — generative differ.
                        "generative" => suitelib::steps::generative(root, &runid),
                        // S4.3 — SQL:2016 coverage ledger check.
                        "sql2016" => suitelib::steps::sql2016(root),
                        // S5.1 — pgbench tpcb-like scoreboard.
                        "pgbench" => suitelib::steps::pgbench(root, &runid),
                        // S5.2 — sysbench MySQL-dialect leg.
                        "sysbench" => suitelib::steps::sysbench(root, &runid),
                        other => Err(format!("internal step `{other}` not wired yet")),
                    },
                });
                // S1.5 — generated-artifact discipline: an ordinary
                // run restores the tracked harness reports the biz
                // step refreshes; only the release flow keeps them
                // (SUITE_KEEP_REPORTS=1) to commit as its chore. A
                // dirty tree after prerelease is what broke a release
                // preflight once.
                if name == "biz" && std::env::var("SUITE_KEEP_REPORTS").is_err() {
                    let _ = std::process::Command::new("git")
                        .args([
                            "checkout",
                            "--",
                            "xtests/sqllogictest/report.json",
                            "xtests/sqllogictest/report.md",
                            "xtests/data_compat/report.md",
                            "xtests/dump_compat/report.md",
                        ])
                        .status();
                }
                match outcome {
                    Ok(note) => println!("  ok    {:<16} {note}", name),
                    Err(e) => {
                        println!("  FAIL  {:<16}", name);
                        eprintln!("{e}");
                        failed = Some(name);
                    }
                }
                idx += 1;
            }
            let report = ledger
                .write(&root.join("target"))
                .expect("write suite report");
            if tier == "full" && failed.is_none() {
                let leaked: Vec<String> =
                    tmp_spg_entries().difference(&tmp_before).cloned().collect();
                if !leaked.is_empty() {
                    println!("  FAIL  tmp-leak         new /tmp survivors: {leaked:?}");
                    failed = Some(format!("tmp-leak: {leaked:?}"));
                }
            }
            let total = t_total.elapsed();
            // v7.39.12 — and what else the machine was doing, because a
            // duration read without it invites the reader to attribute
            // the difference to the code. See `Ledger::load_start`.
            println!(
                "total {total:?} — report {} — load {:.2} -> {:.2}",
                report.display(),
                ledger.load_start,
                ledger.load_end.unwrap_or(-1.0)
            );
            // v7.38.17 — name what did NOT run.
            //
            // `full` holds seven steps that nothing schedules: CI has a
            // push job and a daily drop-in check and neither touches
            // the tier, so those steps run only when a person types
            // them. A release read a green `prerelease` and stopped
            // there, which reads as "everything green" and is not.
            //
            // A total that cannot show what it excluded is a total that
            // overstates — the same failure as a corpus summary that
            // could not show the dialect its files ran in.
            if tier != "full" {
                let ran: std::collections::BTreeSet<&str> =
                    tier_steps.iter().map(|s| s.name.as_str()).collect();
                let skipped: Vec<&str> = m
                    .tier("full")
                    .iter()
                    .map(|s| s.name.as_str())
                    .filter(|n| !ran.contains(n))
                    .collect();
                if !skipped.is_empty() {
                    println!(
                        "NOT RUN ({} full-tier step(s), no schedule runs these): {}",
                        skipped.len(),
                        skipped.join(", ")
                    );
                }
            }
            let mut rc = 0;
            if failed.is_some() {
                rc = 1;
            }
            // The rule is `verdict::judge`, with its evidence on the
            // two factors it uses. It lived inline here through two
            // versions and was wrong in two ways for all of it —
            // nothing in the tree could reach it to say so.
            use suitelib::verdict::{Host, RUNAWAY_FACTOR, SLOWDOWN_FACTOR, Verdict, judge};
            // v7.39.13 — how slow the HOST was, measured in this run.
            //
            // `fmt` parses every source file and formats nothing, so
            // its duration is the machine and not the tree. Against its
            // own median at this band that is a factor every other
            // step's threshold scales by — which is what a fixed budget
            // could never do and a constant would only guess at. It
            // read 6.9 s at a load average of 54.7 against 2.6-3.7 s at
            // around 3, and on that run it failed `fmt` for being the
            // machine it was measuring.
            //
            // The reference itself is skipped, not excused: a ruler
            // compared against itself always reads 1.
            const REFERENCE_STEP: &str = "fmt";
            let host = ledger
                .steps
                .iter()
                .find(|s| s.name == REFERENCE_STEP)
                .map(|s| {
                    let past = suitelib::reportlib::recent_step_ms(
                        &root.join("target"),
                        tier,
                        REFERENCE_STEP,
                        band,
                        6,
                    );
                    let mut sorted = past.clone();
                    sorted.sort_unstable();
                    let median = sorted.get(sorted.len() / 2).map_or(0, |m| u128::from(*m));
                    Host::from_reference(s.duration.as_millis(), median)
                })
                .unwrap_or_default();
            if host != Host::default() {
                eprintln!(
                    "  host         this run is {:.2}x slower than usual, measured on \
                     `{REFERENCE_STEP}` — every threshold below is scaled by it",
                    host.as_ratio()
                );
            }
            for rec in ledger.over_budget() {
                if rec.name == REFERENCE_STEP {
                    continue;
                }
                let name = rec.name.as_str();
                let ms = rec.duration.as_millis();
                let budget = rec.budget.unwrap_or_default();
                let past =
                    suitelib::reportlib::recent_step_ms(&root.join("target"), tier, name, band, 6);
                let mut sorted = past.clone();
                sorted.sort_unstable();
                let median = sorted.get(sorted.len() / 2).map(|m| u128::from(*m));
                let secs: Vec<String> = past
                    .iter()
                    .map(|m| format!("{:.1}s", *m as f64 / 1000.0))
                    .collect();
                let verdict = judge(ms, budget.as_millis(), median, host);
                match verdict {
                    Verdict::Runaway => {
                        eprintln!(
                            "  RUNAWAY      {name} ({:?} > {RUNAWAY_FACTOR}x its {budget:?} \
                             budget) — no history argues this down. Same step at band {band}, \
                             most recent first: {}",
                            rec.duration,
                            if secs.is_empty() {
                                "none".to_string()
                            } else {
                                secs.join(", ")
                            }
                        );
                    }
                    Verdict::Slower => eprintln!(
                        "  SLOWER       {name} ({:?} > {budget:?}, and > {SLOWDOWN_FACTOR}x its \
                         own median {:.1}s) — same step at band {band}, most recent first: {}",
                        rec.duration,
                        median.unwrap_or_default() as f64 / 1000.0,
                        secs.join(", ")
                    ),
                    Verdict::NoHistory => eprintln!(
                        "  over budget  {name} ({:?} > {budget:?}) — \
                         no earlier run at band {band} to compare with, recorded only",
                        rec.duration
                    ),
                    Verdict::HostIsSlow => eprintln!(
                        "  over budget  {name} ({:?} > {budget:?}) — in line with its own \
                         median {:.1}s at band {band}, so this host is slow, not the step: {}",
                        rec.duration,
                        median.unwrap_or_default() as f64 / 1000.0,
                        secs.join(", ")
                    ),
                }
                if verdict.is_red() {
                    rc = 1;
                }
            }
            std::process::exit(rc);
        }
        Some(other) => {
            eprintln!("suite-run: unknown argument '{other}'\n\n{}", usage());
            std::process::exit(2);
        }
    }
}

fn run_cmd(cmd: &str) -> String {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn usage() -> &'static str {
    "suite-run — SPG v7.38 test-suite orchestrator\n\
     \n\
     USAGE: suite-run <TIER> [--json]\n\
     \n\
     TIERS (by speed, each a superset of the last):\n\
       precommit    current-version pins + fastest regressions   (budget 150 s hard)\n\
       prerelease   all regressions + goals + ironrules          (budget 25 min hard)\n\
       full         find problems: no blind spots                (nightly)\n\
     \n\
     Canonical design: .claude/testsuite/ (CHECKLIST.md is the build plan).\n"
}

/// `/tmp` entries with the suite's own prefixes — the leak surface
/// the janitor sweeps and the full tier now asserts on.
fn tmp_spg_entries() -> std::collections::BTreeSet<String> {
    std::fs::read_dir("/tmp")
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("spg-suite-") || n.starts_with("spg-gate-"))
                .collect()
        })
        .unwrap_or_default()
}
