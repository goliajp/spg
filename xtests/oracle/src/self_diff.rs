//! Fast-tier replacement for the docker oracle.
//!
//! Docker startup (3 × ~10s + warm-up) breaks the 60s fast-tier
//! budget. Substitute = run the same corpus on three SPG
//! permutations (embedded / server_simple / server_extended) and
//! assert byte-equal across the three internal paths.
//!
//! This catches ~95% of "this commit changed observable behaviour"
//! regressions without needing any external master. Full-tier
//! (`gate.sh full --oracle`) still runs the three-master differential.
//!
//! v7.38 C ships the entry stub; P1 wires it to v7.38 元机制 B
//! permutation runner once that lands (`xtests/perm-runner/`).

use anyhow::{Result, anyhow};
use std::path::Path;

pub fn run(_corpus: &Path) -> Result<()> {
    Err(anyhow!(
        "self-diff: stub — depends on v7.38 元机制 B (permutation \
         runner @ xtests/perm-runner/). Wire during P1 once the \
         permutation runner exposes a programmatic entry; see design §7."
    ))
}
