//! Round 751 — the round-493 safety gate, moved to its own test target.
//!
//! It reads the process-global `UNIQ_PROBE_*` diagnostic counters, and a
//! process is exactly the scope those counters have. Inside the e2e
//! binary the parallel runner let every concurrently running insert test
//! bump them between this test's two reads, diluting the locators-per-
//! probe ratio — the I06 flake (one failure in three full runs, 5/5
//! green alone). A separate integration-test target runs in its own
//! process, so the ratio is this test's engines and nothing else.
//!
//! Second defect the move fixes: since round 718 (which took
//! `perf-counters` off dogfood_replay so the feature stopped leaking
//! into every workspace build) NO gate build enabled the feature, so the
//! `counted` guard below returned early in every run — the safety gate
//! had silently lost its teeth. The gate runner now invokes this target
//! explicitly with `--features perf-counters`; a separate cargo
//! invocation, so the feature cannot unify into the workspace binaries
//! (the round-718 taint needs one shared build graph).
//!
//! The pinned mechanism is unchanged from round 493: the insert path
//! prunes a churned key's dead index entries ONLY above every live
//! snapshot's horizon. With a snapshot held open it must prune nothing,
//! or a rollback/reader could lose index-reachable rows.

use spg_engine::Engine;

fn seeded() -> Engine {
    let mut e = Engine::new();
    // No background reclamation: this is the server's exposure, and the
    // shape the counters were taken under.
    e.set_autovacuum(false);
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, g INT)").unwrap();
    let mut vals = String::from("INSERT INTO t VALUES ");
    for i in 0..2000 {
        if i > 0 {
            vals.push(',');
        }
        vals.push_str(&format!("({i},{})", i % 7));
    }
    e.execute(&vals).unwrap();
    e
}

#[test]
fn round493_a_held_snapshot_stops_the_pruning() {
    use spg_engine::{UNIQ_PROBE_CALLS, UNIQ_PROBE_LOCATORS};
    use std::sync::atomic::Ordering::Relaxed;

    // Counters only exist under the perf-counters feature; the gate
    // invokes this target with it ON. A plain `cargo test` build says so
    // out loud instead of passing vacuously.
    let counted = {
        let before = UNIQ_PROBE_CALLS.load(Relaxed);
        let mut probe = Engine::new();
        probe.execute("CREATE TABLE c (id INT PRIMARY KEY)").unwrap();
        probe.execute("INSERT INTO c VALUES (1)").unwrap();
        UNIQ_PROBE_CALLS.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "round493 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let churn = |e: &mut Engine, rounds: usize| {
        for c in 0..rounds {
            e.execute("DELETE FROM t WHERE id >= 100 AND id < 200").unwrap();
            let mut re = String::from("INSERT INTO t VALUES ");
            for i in 100..200 {
                if i > 100 {
                    re.push(',');
                }
                re.push_str(&format!("({i},{c})"));
            }
            e.execute(&re).unwrap();
        }
    };
    let locators_per_probe = |e: &mut Engine| -> f64 {
        let base = (
            UNIQ_PROBE_CALLS.load(Relaxed),
            UNIQ_PROBE_LOCATORS.load(Relaxed),
        );
        e.execute("DELETE FROM t WHERE id >= 100 AND id < 200").unwrap();
        let mut re = String::from("INSERT INTO t VALUES ");
        for i in 100..200 {
            if i > 100 {
                re.push(',');
            }
            re.push_str(&format!("({i},7)"));
        }
        e.execute(&re).unwrap();
        let calls = UNIQ_PROBE_CALLS.load(Relaxed) - base.0;
        let locs = UNIQ_PROBE_LOCATORS.load(Relaxed) - base.1;
        assert!(calls > 0, "the uniqueness probe did not run");
        locs as f64 / calls as f64
    };

    // No snapshot held: the horizon is the current version, so a churned
    // key's dead entries go.
    let mut free = seeded();
    churn(&mut free, 20);
    let pruned = locators_per_probe(&mut free);
    assert!(pruned < 3.0, "expected pruning, got {pruned} locators per probe");

    // Snapshot held in another session: the horizon drops to it and the
    // same churn must leave every version in place.
    let mut held = seeded();
    held.set_current_session(1);
    held.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    let _ = held.execute("SELECT count(*) FROM t").unwrap();
    held.set_current_session(2);
    churn(&mut held, 20);
    let kept = locators_per_probe(&mut held);
    assert!(
        kept > 10.0,
        "a held snapshot must stop the pruning, got {kept} locators per probe"
    );
}
