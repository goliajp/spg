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
            let wide = suitelib::steps::changed_crates(root, &graph)
                .map(|c| graph.affected(&c).len() >= 8)
                .unwrap_or(false);
            let band = if tier == "precommit" && wide {
                println!("budget band: BASE (affected ≥ 8 crates; unit ×8, cap 360 s)");
                8u64
            } else {
                1u64
            };
            println!("tier {tier} — {} steps, run {runid}", m.tier(tier).len());
            let mut failed: Option<String> = None;
            let t_total = std::time::Instant::now();
            for s in m.tier(tier) {
                if failed.is_some() {
                    ledger.record_skip(&s.name);
                    println!("  SKIP  {:<16} (earlier step failed)", s.name);
                    continue;
                }
                let budget_secs = s.budget_s.map(|b| {
                    if s.name == "unit-affected" {
                        b * band
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
                        other => Err(format!("internal step `{other}` not wired yet")),
                    },
                });
                match outcome {
                    Ok(note) => println!("  ok    {:<16} {note}", name),
                    Err(e) => {
                        println!("  FAIL  {:<16}", name);
                        eprintln!("{e}");
                        failed = Some(name);
                    }
                }
            }
            let over: Vec<String> = ledger
                .over_budget()
                .iter()
                .map(|r| format!("{} ({:?} > {:?})", r.name, r.duration, r.budget.unwrap()))
                .collect();
            let report = ledger
                .write(&root.join("target"))
                .expect("write suite report");
            let total = t_total.elapsed();
            println!("total {total:?} — report {}", report.display());
            let hard_cap = std::time::Duration::from_secs(if band > 1 { 360 } else { 150 });
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
