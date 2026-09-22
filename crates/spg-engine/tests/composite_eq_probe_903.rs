//! 9.0.3 — a composite index with a text component answered nothing on
//! the collation every shipped image runs, and the seek became a scan.
//!
//! sentori's ingest opens with `SELECT … WHERE project_id = $1 AND
//! fingerprint = $2 FOR UPDATE` over `UNIQUE (project_id, fingerprint)`:
//! 10 ms at 20,000 rows against PostgreSQL 18.6's 0.1 ms, and growing
//! with the table. A composite tree holds tuples of RAW cells, and under
//! a deterministic collation — every collation SPG has — `=` on text is
//! byte equality, so the raw probe is exact.
//!
//! Counted rather than timed, and its own target because the counters are
//! process-global (see `uniq_composite_probe.rs`).

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

#[test]
fn a_text_component_does_not_cost_the_composite_seek() {
    let counted = {
        let before = spg_engine::MULTI_EQ_PROBES.load(Relaxed);
        let mut probe = Engine::new();
        probe
            .execute("CREATE TABLE w (a int, b int, PRIMARY KEY (a, b))")
            .unwrap();
        probe.execute("INSERT INTO w VALUES (1, 1)").unwrap();
        let _ = probe
            .execute("SELECT * FROM w WHERE a = 1 AND b = 1")
            .unwrap();
        spg_engine::MULTI_EQ_PROBES.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "9.0.3 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8").unwrap();
    for sql in [
        "CREATE TABLE issues (id int PRIMARY KEY, project_id int, fingerprint text, \
         UNIQUE (project_id, fingerprint))",
        "INSERT INTO issues SELECT g, 1, 'fp' || g FROM generate_series(1, 3000) g",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    let before = spg_engine::MULTI_EQ_PROBES.load(Relaxed);
    let spg_engine::QueryResult::Rows { rows, .. } = e
        .execute("SELECT id FROM issues WHERE project_id = 1 AND fingerprint = 'fp777'")
        .unwrap()
    else {
        panic!("rows");
    };
    assert_eq!(rows.len(), 1, "the row is still found");
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(777));
    assert!(
        spg_engine::MULTI_EQ_PROBES.load(Relaxed) > before,
        "the composite index answered it, rather than the seek becoming a scan"
    );

    // A folding collation is where a raw probe would MISS, and there the
    // seek still declines: the answer is what matters.
    let mut ci = Engine::new();
    for sql in [
        "CREATE TABLE t (a int, s text COLLATE \"case_insensitive\", PRIMARY KEY (a, s))",
        "INSERT INTO t VALUES (1, 'Row')",
    ] {
        ci.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    let spg_engine::QueryResult::Rows { rows, .. } = ci
        .execute("SELECT s FROM t WHERE a = 1 AND s = 'row'")
        .unwrap()
    else {
        panic!("rows");
    };
    assert_eq!(rows.len(), 1, "a case-insensitive key still matches");
}
