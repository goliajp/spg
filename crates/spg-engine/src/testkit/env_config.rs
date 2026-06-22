//! Test-mode GUC snapshot — frozen at engine init.
//!
//! Hot paths in the engine read fields directly via `engine.env_cfg().<field>`.
//! In production `EnvConfig` is `Default::default()` so every field is `false /
//! None / Auto` — the optimiser can const-fold every gate.
//!
//! Adding a new GUC:
//! 1. Add a field here + default + builder method.
//! 2. Add the read site (`engine.env_cfg().<field>`) at one acceptor in the engine.
//! 3. Add a `tests/env_cfg_<name>.rs` unit pin.
//! 4. Record all three in `xtests/sigil/test-mode-gucs.md`.

/// Frozen snapshot of `SPG_TEST_*` env vars + programmatic overrides.
///
/// Every field defaults to "production behaviour"; only flips when explicitly
/// set via env var (`EnvConfig::from_env`) or builder (`EnvConfig::builder`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvConfig {
    /// `SPG_TEST_COMPUTE_QUERY_ID=regress` — strip query-id annotations
    /// from EXPLAIN output (regression-test mode).
    pub compute_query_id: ComputeQueryId,

    /// `SPG_TEST_EXPLAIN_NO_COSTS=1` — suppress nondeterministic cost /
    /// elapsed annotations in EXPLAIN output. Used so EXPLAIN diffs are
    /// byte-equal across runs and machines.
    pub explain_no_costs: bool,

    /// `SPG_TEST_STATS_FROZEN=1` — freeze the ANALYZE-derived statistics
    /// snapshot. ANALYZE becomes a no-op; INSERT/UPDATE auto-stats refresh
    /// stops firing. Pins cost-model inputs across runs.
    pub stats_frozen: bool,

    /// `SPG_TEST_PLAN_DETERMINISTIC=1` — disable cost-based plan-cache and
    /// join-order decisions; fall back to a lexical / signature-hash tie
    /// break so the chosen plan is invariant w.r.t. stats jitter.
    pub plan_deterministic: bool,

    /// `SPG_TEST_DISABLE_TOPK=1` — disable the aggregate top-K LIMIT
    /// fast path (`select_nth_unstable_by`). Forces the full-sort
    /// fallback so two plans can be diffed for the same ORDER BY +
    /// LIMIT query.
    pub disable_topk: bool,

    /// `SPG_TEST_DISABLE_JOINFOLD=1` — disable the v7.37 joinfold rewrite.
    /// Lets oracle / regression harnesses compare un-folded plans against
    /// the folded baseline.
    pub disable_joinfold: bool,

    /// `SPG_TEST_RANDOM_SEED=N` — seed every nondeterministic source
    /// (hash maps that need a seed, randomised tie-breakers, …) to a
    /// deterministic value. `None` means "production: derive from clock".
    pub random_seed: Option<u64>,
}

/// Semantics of the `compute_query_id` knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeQueryId {
    /// Production: hash + emit query id in EXPLAIN output.
    Auto,
    /// Regression-test mode: elide query id from EXPLAIN output so diffs
    /// are byte-equal across runs.
    Regress,
}

impl Default for ComputeQueryId {
    fn default() -> Self {
        Self::Auto
    }
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            compute_query_id: ComputeQueryId::Auto,
            explain_no_costs: false,
            stats_frozen: false,
            plan_deterministic: false,
            disable_topk: false,
            disable_joinfold: false,
            random_seed: None,
        }
    }
}

impl EnvConfig {
    /// Read every `SPG_TEST_*` env var exactly once. Called by hosts
    /// (spg-server, spg-embedded, tests) at engine init; the engine
    /// itself is `#![no_std]` and never touches env directly on its hot
    /// path.
    ///
    /// Only available when the `std` feature (default) is enabled.
    #[cfg(feature = "std")]
    pub fn from_env() -> Self {
        extern crate std;
        use std::env;

        let mut cfg = Self::default();
        if let Ok(v) = env::var("SPG_TEST_COMPUTE_QUERY_ID") {
            cfg.compute_query_id = if v == "regress" {
                ComputeQueryId::Regress
            } else {
                ComputeQueryId::Auto
            };
        }
        if env_flag("SPG_TEST_EXPLAIN_NO_COSTS") {
            cfg.explain_no_costs = true;
        }
        if env_flag("SPG_TEST_STATS_FROZEN") {
            cfg.stats_frozen = true;
        }
        if env_flag("SPG_TEST_PLAN_DETERMINISTIC") {
            cfg.plan_deterministic = true;
        }
        if env_flag("SPG_TEST_DISABLE_TOPK") {
            cfg.disable_topk = true;
        }
        if env_flag("SPG_TEST_DISABLE_JOINFOLD") {
            cfg.disable_joinfold = true;
        }
        if let Ok(v) = env::var("SPG_TEST_RANDOM_SEED") {
            cfg.random_seed = v.parse().ok();
        }
        cfg
    }

    /// Builder-style override for programmatic GUC injection (used by
    /// permutation runner / oracle runner / unit tests that build an
    /// engine without env vars).
    #[must_use]
    pub fn builder() -> EnvConfigBuilder {
        EnvConfigBuilder { cfg: Self::default() }
    }
}

#[cfg(feature = "std")]
fn env_flag(name: &str) -> bool {
    extern crate std;
    std::env::var(name).map(|v| v == "1").unwrap_or(false)
}

/// Builder for `EnvConfig`. One method per field; symmetric with the env
/// vars in `from_env`.
#[derive(Debug, Clone, Default)]
pub struct EnvConfigBuilder {
    cfg: EnvConfig,
}

impl EnvConfigBuilder {
    pub fn compute_query_id(mut self, v: ComputeQueryId) -> Self {
        self.cfg.compute_query_id = v;
        self
    }
    pub fn explain_no_costs(mut self, v: bool) -> Self {
        self.cfg.explain_no_costs = v;
        self
    }
    pub fn stats_frozen(mut self, v: bool) -> Self {
        self.cfg.stats_frozen = v;
        self
    }
    pub fn plan_deterministic(mut self, v: bool) -> Self {
        self.cfg.plan_deterministic = v;
        self
    }
    pub fn disable_topk(mut self, v: bool) -> Self {
        self.cfg.disable_topk = v;
        self
    }
    pub fn disable_joinfold(mut self, v: bool) -> Self {
        self.cfg.disable_joinfold = v;
        self
    }
    pub fn random_seed(mut self, v: u64) -> Self {
        self.cfg.random_seed = Some(v);
        self
    }
    pub fn build(self) -> EnvConfig {
        self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_production() {
        let cfg = EnvConfig::default();
        assert_eq!(cfg.compute_query_id, ComputeQueryId::Auto);
        assert!(!cfg.explain_no_costs);
        assert!(!cfg.stats_frozen);
        assert!(!cfg.plan_deterministic);
        assert!(!cfg.disable_topk);
        assert!(!cfg.disable_joinfold);
        assert_eq!(cfg.random_seed, None);
    }

    #[test]
    fn builder_roundtrip() {
        let cfg = EnvConfig::builder()
            .explain_no_costs(true)
            .disable_topk(true)
            .random_seed(42)
            .build();
        assert!(cfg.explain_no_costs);
        assert!(cfg.disable_topk);
        assert_eq!(cfg.random_seed, Some(42));
        // Untouched fields stay at default.
        assert!(!cfg.disable_joinfold);
        assert_eq!(cfg.compute_query_id, ComputeQueryId::Auto);
    }
}
