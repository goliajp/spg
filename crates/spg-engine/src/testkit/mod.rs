//! Test-mode GUC framework (v7.38 P0 元机制 D).
//!
//! Frozen-at-init env-var snapshot that lets tests pin nondeterministic
//! surfaces (cost-based plans, EXPLAIN timing, RNG seeds, …) without
//! resorting to `#[ignore]` / retry. Production builds default every
//! field to its `EnvConfig::default()` value — the optimiser can
//! const-fold every `if env_cfg.<field>` branch away.
//!
//! Design doc: `.claude/notes/v7.38-test-mode-gucs-design.md`.
//! Index of acceptors / tests: `xtests/sigil/test-mode-gucs.md`.

pub mod env_config;
pub mod injection;

pub use env_config::{ComputeQueryId, EnvConfig, EnvConfigBuilder};
