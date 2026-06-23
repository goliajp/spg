//! v7.38 元机制 D unit pin — `SPG_TEST_RANDOM_SEED=N` pins the engine's
//! single RNG seed source (`Engine::rng_seed`). Two engines built with
//! the same seed return identical `rng_seed()`; engines without a
//! seed-installed clock fall back to a fixed sentinel rather than wall
//! time.
//!
//! Acceptor: `crates/spg-engine/src/lib.rs` `Engine::rng_seed`.
//! Index   : `xtests/sigil/test-mode-gucs.md`.

use spg_engine::testkit::EnvConfig;
use spg_engine::Engine;

#[test]
fn random_seed_is_byte_equal_with_same_seed() {
    let cfg = EnvConfig::builder().random_seed(42).build();
    let a = Engine::new().with_env_cfg(cfg.clone());
    let b = Engine::new().with_env_cfg(cfg);
    assert_eq!(a.rng_seed(), 42);
    assert_eq!(a.rng_seed(), b.rng_seed());
}

#[test]
fn random_seed_distinct_across_distinct_seeds() {
    let a = Engine::new().with_env_cfg(EnvConfig::builder().random_seed(1).build());
    let b = Engine::new().with_env_cfg(EnvConfig::builder().random_seed(99).build());
    assert_ne!(a.rng_seed(), b.rng_seed());
    assert_eq!(a.rng_seed(), 1);
    assert_eq!(b.rng_seed(), 99);
}

#[test]
fn rng_seed_falls_back_to_clock_when_unset() {
    // Production-mode engine (no GUC) reads the clock. Use a stub
    // clock to assert the path is actually consulted instead of
    // hitting the no-clock sentinel.
    fn clock() -> i64 {
        12345
    }
    let e = Engine::new().with_clock(clock);
    assert_eq!(e.rng_seed(), 12345);
}

#[test]
fn rng_seed_uses_sentinel_when_no_clock_no_seed() {
    let e = Engine::new();
    // Sentinel matches `Engine::rng_seed` so production engines without
    // a wall-clock installed still get a deterministic, non-zero seed.
    assert_eq!(e.rng_seed(), 0xBAD_5EED_DEAD_BEEFu64);
}
