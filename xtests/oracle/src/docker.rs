//! docker-compose orchestration for the three reference masters.
//!
//! Shells out to `docker compose` with the harness's compose file —
//! the same file gate.sh uses directly. Kept thin on purpose: the
//! compose file is the single source of truth for images (D13 pins),
//! ports, and healthchecks; this module only starts/stops it.

use anyhow::{Context, Result, bail};
use std::process::Command;

const COMPOSE_FILE: &str = "xtests/oracle/docker-compose.yml";

fn compose(args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(COMPOSE_FILE)
        .args(args)
        .status()
        .context("spawn docker compose (is docker on PATH? OrbStack: /Applications/OrbStack.app/Contents/MacOS/xbin)")?;
    if !status.success() {
        bail!("docker compose {args:?} exited {status}");
    }
    Ok(())
}

/// Bring up all three oracle services and wait for healthy.
///
/// v7.38.25 — `--build`. The header above calls this compose file the
/// single source of truth for which reference masters SPG is measured
/// against, and it was not: `up` without `--build` reuses whatever
/// `spg-oracle-*:v7.38` is already on the machine, so raising a base
/// version in `mysql/Dockerfile` or `mariadb/Dockerfile` changed the
/// file and nothing else. The Dockerfile would have said one version
/// while the container answering the differential ran another, and the
/// run would have reported no difference at all.
///
/// With the build layer cache this costs a second or two per run and
/// rebuilds only when a Dockerfile actually moved.
pub fn up() -> Result<()> {
    compose(&["up", "-d", "--wait", "--build"])
}

/// Tear down the docker-compose stack.
pub fn down() -> Result<()> {
    compose(&["down", "-v"])
}
