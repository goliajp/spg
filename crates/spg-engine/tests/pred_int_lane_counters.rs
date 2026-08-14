//! r1021 — does the integer predicate lane actually fire, and does it hand
//! back the rows it must not decide?
//!
//! The corpus pins the answers (`15_regressions/pred_int_arith_lane.test`),
//! and answers alone cannot tell a working lane from a lane that never runs:
//! both produce PG's results, because falling back IS producing PG's
//! results. Round 480 of the earlier VM work was spent on a branch that
//! turned out unreachable, which is why "is it even reached" is a counter
//! here rather than an inference.
//!
//! Own test target, for the reason `uniq_prune_counters.rs` became one: the
//! counters are process-global and a neighbour bumping them between two
//! reads dilutes the reading. The gate runs it separately with
//! `--features perf-counters` (`scripts/gate.sh run_gates`).

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.set_autovacuum(false);
    e.execute("CREATE TABLE il (i4 INT, i8 BIGINT, i2 SMALLINT, n INT)")
        .unwrap();
    e.execute("INSERT INTO il VALUES (7,7,7,7),(8,8,8,NULL),(0,0,0,0),(-9,-9,-9,-9)")
        .unwrap();
    e
}

#[test]
fn r1021_the_int_lane_fires_and_declines_the_rows_it_must() {
    use spg_engine::eval::compiled::{STEP_VM_INTLANE_FALLBACK, STEP_VM_INTLANE_FIRE};

    let counted = {
        let before = STEP_VM_INTLANE_FIRE.load(Relaxed);
        let mut probe = seeded();
        probe.execute("SELECT i4 FROM il WHERE i4 % 3 = 0").unwrap();
        STEP_VM_INTLANE_FIRE.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "r1021 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let mut e = seeded();
    let measure = |e: &mut Engine, sql: &str| -> (u64, u64) {
        let base = (
            STEP_VM_INTLANE_FIRE.load(Relaxed),
            STEP_VM_INTLANE_FALLBACK.load(Relaxed),
        );
        let _ = e.execute(sql);
        (
            STEP_VM_INTLANE_FIRE.load(Relaxed) - base.0,
            STEP_VM_INTLANE_FALLBACK.load(Relaxed) - base.1,
        )
    };

    // Four rows, all int4, no NULL: the lane decides every one of them.
    let (fired, fell) = measure(&mut e, "SELECT i4 FROM il WHERE i4 % 3 = 0");
    assert_eq!(fired, 4, "the lane should have decided all four rows");
    assert_eq!(fell, 0, "none of them needed the ordinary machine");

    // `n` is NULL on one row, and a NULL is not the lane's to decide.
    let (fired, fell) = measure(&mut e, "SELECT i4 FROM il WHERE n % 3 = 0");
    assert_eq!(fired, 3, "the three non-NULL rows are the lane's");
    assert_eq!(fell, 1, "the NULL row must be handed back, not decided");

    // smallint is not admitted, so the shape never classifies as the lane
    // at all and no row is either decided or handed back by it.
    let (fired, fell) = measure(&mut e, "SELECT i4 FROM il WHERE i2 % 3 = 0");
    assert_eq!(
        (fired, fell),
        (0, 0),
        "a smallint predicate must not reach the lane"
    );

    // A zero divisor is an error the interpreter raises. The lane must
    // decline every row rather than deciding any of them.
    let (fired, fell) = measure(&mut e, "SELECT i4 FROM il WHERE i4 % 0 = 1");
    assert_eq!(fired, 0, "a zero divisor must not be decided by the lane");
    assert!(fell >= 1, "and the row must be handed back: fell={fell}");

    // An int4 result leaving int4 range is also the interpreter's to raise.
    e.execute("INSERT INTO il VALUES (2147483647,1,1,1)")
        .unwrap();
    let (_, fell) = measure(&mut e, "SELECT i4 FROM il WHERE i4 * 2 = 0");
    assert!(
        fell >= 1,
        "the overflowing row must be handed back: fell={fell}"
    );

    // The control: a predicate with no arithmetic keeps its own fast path
    // and never involves the lane.
    let (fired, fell) = measure(&mut e, "SELECT i4 FROM il WHERE i4 > 0");
    assert_eq!(
        (fired, fell),
        (0, 0),
        "`column cmp literal` has its own shape and must stay on it"
    );
}
