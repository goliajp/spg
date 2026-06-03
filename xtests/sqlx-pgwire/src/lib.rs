//! sqlx 0.8 ↔ spg-server PG-wire integration suite.
//!
//! Crate is `publish = false` so it never reaches crates.io.
//! Lives alongside `xbench/competitor` and `xtests/sqllogictest`
//! as a workspace test harness.
//!
//! ## Running
//!
//! 1. Start a spg-server with PG-wire enabled:
//!
//!    ```bash
//!    docker run -d --name spg-sqlxtest \
//!        -p 5433:5432 \
//!        -e SPG_PG_ADDR=0.0.0.0:5432 \
//!        -v spg-sqlxtest:/data \
//!        goliakk/spg:7.9.0
//!    ```
//!
//! 2. Export the URL the tests read:
//!
//!    ```bash
//!    export SPG_PG_URL='postgres://bench:bench@127.0.0.1:5433/bench'
//!    ```
//!
//! 3. Run the suite:
//!
//!    ```bash
//!    cargo test -p spg-sqlx-pgwire -- --ignored
//!    ```
//!
//! Without `SPG_PG_URL` set the tests are gated behind
//! `#[ignore]` so a plain `cargo test --workspace` is unaffected.
//!
//! ## Coverage
//!
//! - JSONB INSERT + SELECT round-trip via `serde_json::Value`
//! - TIMESTAMPTZ INSERT + SELECT into `chrono::DateTime<Utc>`
//! - INSERT … RETURNING id (BIGSERIAL surrogate)
//! - INSERT … ON CONFLICT (col) DO NOTHING
//! - INSERT … ON CONFLICT (col) DO UPDATE SET … (EXCLUDED.col)
//! - INSERT … ON CONFLICT (composite) DO UPDATE …

#![doc(html_root_url = "https://docs.rs/spg-sqlx-pgwire/0.0.0")]
