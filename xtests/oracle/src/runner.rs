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

/// Entry point used by `Cmd::Run` and `Cmd::All`.
pub fn run_all(corpus: &Path, expected: &Path, oracle: Oracle) -> Result<()> {
    let grouped = naming::list(corpus)?;
    let pipeline = AdjustPipeline::standard();
    let mut total = 0usize;
    let mut failed = 0usize;

    for (kind, fixtures) in &grouped {
        if *kind == Kind::Depd {
            // depd.* are setup-only, never run standalone.
            continue;
        }
        for fixture in fixtures {
            total += 1;
            match run_one(fixture, expected, oracle, &pipeline) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("FAIL {}: {e:#}", fixture.display());
                    failed += 1;
                }
            }
        }
    }

    println!(
        "oracle={oracle:?} fixtures={total} failed={failed} ({:.1}% pass)",
        if total == 0 {
            100.0
        } else {
            100.0 * (total - failed) as f64 / total as f64
        }
    );
    if failed > 0 {
        bail!("{failed}/{total} fixtures failed on oracle {oracle:?}");
    }
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

    let spg_raw = run_on_spg(fixture)
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

/// Execute a fixture on SPG using the embedded path.
///
/// **Stub.** v7.38 C ships the call shape; P1 wires it to
/// `spg-engine::Engine::execute` + result-set serialiser.
fn run_on_spg(_fixture: &Path) -> Result<String> {
    Err(anyhow!(
        "run_on_spg: stub — wire spg-engine embedded path during \
         v7.38 P1 fill (see design §6.1 step 2)"
    ))
}

/// Execute a fixture on a reference master via sqlx.
///
/// **Stub.** v7.38 C ships the call shape; P1 wires sqlx
/// `query.execute` + dialect-specific result serialiser.
#[allow(dead_code)]
fn run_on_oracle(_fixture: &Path, _oracle: Oracle) -> Result<String> {
    Err(anyhow!(
        "run_on_oracle: stub — wire sqlx adapter (PG/MySQL/MariaDB) \
         during v7.38 P1 fill"
    ))
}
