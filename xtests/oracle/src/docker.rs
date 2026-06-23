//! docker-compose orchestration for the three reference masters.
//!
//! v7.38 C scaffolding: only the CLI surface is wired. Real
//! `docker compose up --wait` shell-out + healthcheck polling lands
//! during P1 fill alongside the corpus that actually drives the
//! oracles.
//!
//! Kept as a small surface area on purpose — production CI shells
//! out to `docker compose` directly (gate.sh) and only loops back
//! here for ad-hoc developer runs.

use anyhow::{Result, anyhow};

/// Bring up all three oracle services and wait for healthy.
pub fn up() -> Result<()> {
    Err(anyhow!(
        "docker up: not implemented in v7.38 C scaffolding — wire to \
         `docker compose -f xtests/oracle/docker-compose.yml up -d --wait` \
         during P1 fill (see design §11)"
    ))
}

/// Tear down the docker-compose stack.
pub fn down() -> Result<()> {
    Err(anyhow!(
        "docker down: not implemented in v7.38 C scaffolding — wire to \
         `docker compose -f xtests/oracle/docker-compose.yml down -v` \
         during P1 fill"
    ))
}
