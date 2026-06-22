//! v7.38 P0 元机制 D — `SPG_TEST_STATS_FROZEN=1` acceptor pin.
//! When set, `ANALYZE` is a no-op: the per-table modified-row
//! counter never resets and the statistic version never bumps,
//! so plan-cache entries stay valid across the call. A
//! regression test that asserts "same plan across runs" needs
//! this so a stray `ANALYZE` doesn't silently invalidate its
//! cached plan.

use spg_engine::{Engine, QueryResult, testkit::EnvConfig};

fn build_engine_with(frozen: bool) -> Engine {
    let cfg = EnvConfig::builder().stats_frozen(frozen).build();
    let mut e = Engine::new().with_env_cfg(cfg);
    e.execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    e
}

#[test]
fn stats_frozen_makes_analyze_return_no_op_command_ok() {
    let mut e = build_engine_with(true);
    let r = e.execute("ANALYZE t").unwrap();
    match r {
        QueryResult::CommandOk {
            affected,
            modified_catalog,
        } => {
            assert_eq!(
                affected, 0,
                "frozen ANALYZE must report affected=0 (true no-op)"
            );
            assert!(
                !modified_catalog,
                "frozen ANALYZE must not flag catalog dirty"
            );
        }
        other => panic!("expected CommandOk, got {other:?}"),
    }
}

#[test]
fn stats_frozen_off_default_runs_analyze_normally() {
    let mut e = build_engine_with(false);
    // Production default: ANALYZE returns a successful CommandOk
    // but with modified_catalog=true so the embedding layer knows
    // to persist the new statistics.
    let r = e.execute("ANALYZE t").unwrap();
    match r {
        QueryResult::CommandOk {
            modified_catalog, ..
        } => {
            assert!(
                modified_catalog,
                "production ANALYZE must flag the catalog dirty so persistence picks it up"
            );
        }
        other => panic!("expected CommandOk, got {other:?}"),
    }
}
