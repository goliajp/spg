//! v7.38 current-goal pins — the `pin_v738_` group (suite design D5).
//!
//! The precommit tier runs exactly this filter:
//!
//!     cargo test -p spg-engine --test e2e pin_v738_
//!
//! When 7.39 begins, `current_pin_prefix` in xtests/suite.toml moves
//! on and these tests KEEP their names — history stays runnable.

use spg_engine::Engine;
use spg_engine::testkit::EnvConfig;

/// 7.38-D — the test-mode GUC framework is alive END TO END: a seeded
/// engine's `random()` is deterministic through actual SQL, not just
/// through the `rng_seed()` accessor the unit pin reads. The GUC index
/// (xtests/sigil/test-mode-gucs.md) is the source of truth this pin
/// keeps honest at the query level.
#[test]
fn pin_v738_test_mode_guc_framework_is_live() {
    let draw = || -> String {
        let mut e = Engine::new().with_env_cfg(EnvConfig::builder().random_seed(42).build());
        match e.execute("SELECT floor(random() * 1000000)::int").unwrap() {
            spg_engine::QueryResult::Rows { rows, .. } => {
                spg_engine::eval::value_to_text(&rows[0].values[0])
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        draw(),
        draw(),
        "same seed, same draw — the GUC is not reaching random()"
    );
}
