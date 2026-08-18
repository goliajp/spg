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
                "clippy-affected" => suitelib::steps::clippy_affected(root, &graph),
                "unit-affected" => suitelib::steps::unit_affected(root, &graph),
                "ironrule-smoke" => suitelib::steps::ironrule_smoke(root, &runid),
                "ironrules" => suitelib::steps::ironrules_full(root, &runid),
                "perf-sweep" => suitelib::steps::perf_sweep(root, &runid),
                "perm-matrix" => suitelib::steps::perm_matrix(root),
                "oracle-three" => suitelib::steps::oracle_three(root),
                "sql2016" => suitelib::steps::sql2016(root),
                "pgbench" => suitelib::steps::pgbench(root, &runid),
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
            const HEAVY: [&str; 5] = [
                "spg-engine",
                "spg-server",
                "spg-storage",
                "spg-sql",
                "suitelib",
            ];
            let wide = suitelib::steps::changed_crates(root, &graph)
                .map(|c| {
                    let a = graph.affected(&c);
                    a.len() >= 8 || a.iter().any(|x| HEAVY.contains(&x.as_str()))
                })
                .unwrap_or(false);
            let band = if tier == "precommit" && wide {
                println!("budget band: BASE (affected ≥ 8 crates; clippy/unit ×8×1.2, cap 480 s)");
                8u64
            } else {
                1u64
            };
            println!("tier {tier} — {} steps, run {runid}", m.tier(tier).len());
            // S4.4 — /tmp leak assertion (the S2.4 half deferred here):
            // full tier snapshots /tmp/spg-* before its steps and any
            // NEW survivor at the end is a red, not a shrug.
            let tmp_before: std::collections::BTreeSet<String> = if tier == "full" {
                tmp_spg_entries()
            } else {
                std::collections::BTreeSet::new()
            };
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
                            "unit-affected" | "clippy-affected" | "pins-current" | "slt-smoke"
                        )
                    {
                        b * band * 12 / 10
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
                        "clippy-affected" => suitelib::steps::clippy_affected(root, &graph),
                        "unit-affected" => suitelib::steps::unit_affected(root, &graph),
                        "ironrule-smoke" => suitelib::steps::ironrule_smoke(root, &runid),
                        // S1.3 — smoke plus the previous release's data
                        // directory opened by the current binary.
                        "ironrules" => suitelib::steps::ironrules_full(root, &runid),
                        "perf-sweep" => suitelib::steps::perf_sweep(root, &runid),
                        // full tier (CP3) — the two 元机制 carriers.
                        "perm-matrix" => suitelib::steps::perm_matrix(root),
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
            let over: Vec<String> = ledger
                .over_budget()
                .iter()
                .map(|r| format!("{} ({:?} > {:?})", r.name, r.duration, r.budget.unwrap()))
                .collect();
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
            println!("total {total:?} — report {}", report.display());
            let hard_cap = std::time::Duration::from_secs(if band > 1 { 480 } else { 150 });
            let mut rc = 0;
            if failed.is_some() {
                rc = 1;
            }
            if !over.is_empty() {
                if tier == "precommit" {
                    eprintln!("suite-run: OVER BUDGET (precommit budgets are hard, D25): {over:?}");
                    rc = 1;
                } else {
                    eprintln!("suite-run: over budget (recorded, not blocking): {over:?}");
                }
            }
            if tier == "precommit" && total > hard_cap {
                eprintln!("suite-run: precommit total {total:?} exceeds the 150 s hard cap");
                rc = 1;
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
