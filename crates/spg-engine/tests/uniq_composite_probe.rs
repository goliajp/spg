//! r1018 — its own test target, for the reason `uniq_prune_counters.rs`
//! became one: `UNIQ_PROBE_*` are process-global, so a second test bumping
//! them between this one's two reads dilutes the ratio into meaninglessness.
//! Measured while writing this: run beside the round-493 test in one target,
//! the reading came back 4.5 locators per probe against a true 0, and the
//! assertion failed for the apparatus rather than the code.
//!
//! The gate runner invokes this target separately with `--features
//! perf-counters` (`scripts/gate.sh run_gates`), which is also what keeps the
//! feature out of the workspace build graph.

use spg_engine::Engine;

/// r1018 — a composite UNIQUE whose leading column does not discriminate
/// must not walk the table once per inserted row.
///
/// mailrs (2026-08-13) reported a 98 MB dump that PostgreSQL 18 loads in
/// 10.9 s and `spg import` had not finished after forty minutes. The cost
/// was proportional to rows already present: their `UNIQUE(mailbox_id, uid)`
/// on a single-mailbox table made the btree probe's leading-column descent
/// select every row, and each candidate was then re-folded on the full key.
/// Measured before the fix: 4,750 locators per probe at 9,500 rows, growing
/// exactly in step with the table.
///
/// The assertion is on locators, not on elapsed time. A timing bound would
/// have to be loose enough to survive a shared machine, and this defect is a
/// factor of thousands — but locators are what the defect IS, and they are
/// counted rather than sampled.
#[test]
fn r1018_a_scope_leading_composite_unique_does_not_scan_the_table() {
    use spg_engine::{UNIQ_PROBE_CALLS, UNIQ_PROBE_LOCATORS};
    use std::sync::atomic::Ordering::Relaxed;

    let counted = {
        let before = UNIQ_PROBE_CALLS.load(Relaxed);
        let mut probe = Engine::new();
        probe
            .execute("CREATE TABLE c2 (id INT PRIMARY KEY)")
            .unwrap();
        probe.execute("INSERT INTO c2 VALUES (1)").unwrap();
        UNIQ_PROBE_CALLS.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "r1018 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let batch = |lo: i64, hi: i64| {
        let mut s = String::from("INSERT INTO m VALUES ");
        for (k, id) in (lo..hi).enumerate() {
            if k > 0 {
                s.push(',');
            }
            // Leading column constant — one mailbox, mailrs's shape.
            s.push_str(&format!("({id},1,{id})"));
        }
        s
    };

    let mut e = Engine::new();
    e.set_autovacuum(false);
    e.execute(
        "CREATE TABLE m (id BIGINT PRIMARY KEY, mailbox_id BIGINT, uid BIGINT, \
         UNIQUE(mailbox_id, uid))",
    )
    .unwrap();
    for b in 0..8 {
        e.execute(&batch(b * 500 + 1, (b + 1) * 500 + 1)).unwrap();
    }

    // The table now holds 4,000 rows sharing one leading value. Insert 500
    // more and count what enforcing the constraint had to look at.
    let base = (
        UNIQ_PROBE_CALLS.load(Relaxed),
        UNIQ_PROBE_LOCATORS.load(Relaxed),
    );
    e.execute(&batch(4001, 4501)).unwrap();
    let calls = UNIQ_PROBE_CALLS.load(Relaxed) - base.0;
    let locs = UNIQ_PROBE_LOCATORS.load(Relaxed) - base.1;

    // The PRIMARY KEY probes too, and its column is unique, so calls are
    // non-zero and its locators are zero. Anything the composite constraint
    // contributes shows up on top of that.
    assert!(calls > 0, "no uniqueness probe ran at all");
    let per_probe = locs as f64 / calls as f64;
    assert!(
        per_probe < 4.0,
        "the probe walked {per_probe:.0} locators per call on a 4,000-row \
         table ({locs} over {calls} probes) — it is descending on the \
         non-discriminating leading column again"
    );
}
