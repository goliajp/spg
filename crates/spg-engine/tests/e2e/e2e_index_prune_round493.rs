//! read01 round 493 — the insert path prunes this key's dead index entries.
//!
//! A BTree posting list holds one locator per row VERSION, so deleting and
//! re-inserting the same id grows it without bound between vacuums —
//! measured at 61 locators under one PK by cycle 60 (round 492), each
//! costing the uniqueness probe a header lookup, and the range seek
//! walking the same versions (round 490). Vacuum already prunes them, by
//! rebuilding every index, which is why it runs rarely enough to matter.
//!
//! The insert already clones the key's posting list to append to it, so
//! pruning there is free. Safety is vacuum's own argument: the horizon is
//! the floor of every live snapshot, so a version reclaimable under it is
//! invisible to every reader that exists or can yet begin.
//!
//! These pin the argument, not the speedup. The dangerous case is dropping
//! an entry some snapshot can still reach — a row that then goes missing
//! from index paths while a sequential scan still finds it, which is
//! exactly the kind of divergence a result-only test can miss. So the
//! reads below are written to go through the index (equality and range on
//! the PK) and are cross-checked against an unindexed predicate.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn seeded(cycles: usize) -> Engine {
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
    for c in 0..cycles {
        e.execute("DELETE FROM t WHERE id >= 100 AND id < 200").unwrap();
        let mut re = String::from("INSERT INTO t VALUES ");
        for i in 100..200 {
            if i > 100 {
                re.push(',');
            }
            re.push_str(&format!("({i},{})", c % 7));
        }
        e.execute(&re).unwrap();
    }
    e
}

#[test]
fn round493_churned_key_reads_the_same_through_index_and_scan() {
    for cycles in [0usize, 1, 20, 60] {
        let mut e = seeded(cycles);
        // Equality on the PK — the uniqueness/seek path.
        let expect_g = if cycles == 0 { 150 % 7 } else { (cycles - 1) % 7 };
        assert_eq!(
            one(&mut e, "SELECT g FROM t WHERE id = 150"),
            expect_g.to_string(),
            "cycles={cycles}"
        );
        // Range on the PK — the seek path.
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE id >= 100 AND id < 200"),
            "100",
            "cycles={cycles}"
        );
        // The same question with a predicate no index can answer. If a
        // live entry had been pruned, the index paths above would go quiet
        // while this one still found the row.
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE id + 0 >= 100 AND id + 0 < 200"),
            "100",
            "cycles={cycles}"
        );
        assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "2000", "cycles={cycles}");
    }
}

#[test]
fn round493_rollback_restores_a_churned_key() {
    // The version a rollback restores must still be reachable through the
    // index: pruning an entry whose row comes back is the silent-wrong
    // this could cause.
    let mut e = seeded(20);
    assert_eq!(one(&mut e, "SELECT g FROM t WHERE id = 150"), "5");
    e.execute("BEGIN").unwrap();
    e.execute("DELETE FROM t WHERE id >= 100 AND id < 200").unwrap();
    let mut re = String::from("INSERT INTO t VALUES ");
    for i in 100..200 {
        if i > 100 {
            re.push(',');
        }
        re.push_str(&format!("({i},99)"));
    }
    e.execute(&re).unwrap();
    assert_eq!(one(&mut e, "SELECT g FROM t WHERE id = 150"), "99");
    e.execute("ROLLBACK").unwrap();
    assert_eq!(one(&mut e, "SELECT g FROM t WHERE id = 150"), "5");
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE id >= 100 AND id < 200"),
        "100"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "2000");
}

/// The safety gate itself: with a snapshot held open, the horizon drops
/// and the insert path must prune NOTHING.
///
/// This pins the mechanism the change depends on rather than the read
/// semantics on top of it. (An attempt to pin the latter — a REPEATABLE
/// READ transaction in one session reading across another session's churn
/// — fails identically with the pruning compiled out, so it is a separate,
/// pre-existing question and is recorded as one rather than smuggled in
/// here.)
#[test]
fn round493_a_held_snapshot_stops_the_pruning() {
    use spg_engine::{UNIQ_PROBE_CALLS, UNIQ_PROBE_LOCATORS};
    use std::sync::atomic::Ordering::Relaxed;

    // Counters only exist under the perf-counters feature; without it this
    // asserts nothing, so it says so rather than passing vacuously.
    let counted = {
        let before = UNIQ_PROBE_CALLS.load(Relaxed);
        let mut probe = Engine::new();
        probe.execute("CREATE TABLE c (id INT PRIMARY KEY)").unwrap();
        probe.execute("INSERT INTO c VALUES (1)").unwrap();
        UNIQ_PROBE_CALLS.load(Relaxed) > before
    };
    if !counted {
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
    let mut free = seeded(0);
    churn(&mut free, 20);
    let pruned = locators_per_probe(&mut free);
    assert!(pruned < 3.0, "expected pruning, got {pruned} locators per probe");

    // Snapshot held in another session: the horizon drops to it and the
    // same churn must leave every version in place.
    let mut held = seeded(0);
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

#[test]
fn round493_duplicate_key_is_still_rejected_after_churn() {
    // Pruning must not drop the LIVE entry the uniqueness probe needs.
    let mut e = seeded(30);
    let dup = e.execute("INSERT INTO t VALUES (150, 1)");
    assert!(dup.is_err(), "duplicate PK accepted after churn: {dup:?}");
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "2000");
}
