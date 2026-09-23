//! 9.0.4 — a transaction's per-statement rebase must not retire the
//! relation's indexes.
//!
//! READ COMMITTED gives every statement the latest committed data, so a
//! transaction whose base moved rebuilds its shadow and REPLAYS its own
//! writes onto it. The replay inserted those rows without their index
//! keys, and an expression index or a locale-collated one that cannot be
//! keyed drops out of service — so the next statement rebuilt it from
//! every row.
//!
//! Nothing happens at one client: the base does not move, and the rebase
//! is skipped. At two it runs per statement, and the customer's `issues`
//! — `UNIQUE (project_id, fingerprint)` on a text column, under the
//! collation every shipped image runs — was retired and rebuilt several
//! times per transaction.
//!
//! Measured through sentori's ingest transaction against PostgreSQL 18.6
//! on the published 9.0.3, same limits, same client:
//!
//! ```text
//!   clients   SPG        PostgreSQL
//!   1         ~310 tps   ~510 tps
//!   4         ~40 tps    ~1400 tps      SPG FALLS as clients are added
//!   8         ~24 tps    ~1700 tps
//! ```
//!
//! Counted rather than timed: the rebuilds ARE the defect, and a timing
//! bound loose enough for a shared machine could not see them reliably.

use spg_engine::{Engine, QueryResult};
use std::sync::atomic::Ordering::Relaxed;

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: not rows");
    };
    rows.iter().map(|r| format!("{:?}", r.values)).collect()
}

#[test]
fn a_rebase_does_not_retire_the_collated_index() {
    let counted = {
        let before = spg_engine::INDEX_REBUILDS.load(Relaxed);
        let mut probe = Engine::new();
        probe.set_database_collation("en_US.utf8").unwrap();
        probe
            .execute("CREATE TABLE w (id int PRIMARY KEY, s text UNIQUE)")
            .unwrap();
        probe.execute("INSERT INTO w VALUES (1, 'a')").unwrap();
        probe.execute("UPDATE w SET s = 'b' WHERE id = 1").unwrap();
        spg_engine::INDEX_REBUILDS.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "9.0.4 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8").unwrap();
    for sql in [
        "CREATE TABLE issues (id int PRIMARY KEY, project_id int, fingerprint text, \
         n bigint NOT NULL DEFAULT 0, UNIQUE (project_id, fingerprint))",
        "INSERT INTO issues SELECT g, 1, 'fp' || g, 0 FROM generate_series(1, 400) g",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }

    // Two transactions, interleaved on their own slots — the shape a
    // second client makes. B's COMMIT moves the base, so A's next
    // statement rebases.
    let a = e.alloc_tx_id();
    let b = e.alloc_tx_id();
    e.execute_in("BEGIN", a).unwrap();
    e.execute_in("UPDATE issues SET n = n + 1 WHERE id = 1", a)
        .unwrap();

    let before = spg_engine::INDEX_REBUILDS.load(Relaxed);
    for round in 0..4 {
        e.execute_in("BEGIN", b).unwrap();
        e.execute_in(
            &format!("UPDATE issues SET n = n + 1 WHERE id = {}", 100 + round),
            b,
        )
        .unwrap();
        e.execute_in("COMMIT", b).unwrap();
        // A's base has moved: this statement rebases before it runs.
        e.execute_in(
            &format!("UPDATE issues SET n = n + 1 WHERE id = {}", 2 + round),
            a,
        )
        .unwrap();
    }
    e.execute_in("COMMIT", a).unwrap();

    assert_eq!(
        spg_engine::INDEX_REBUILDS.load(Relaxed) - before,
        0,
        "the rebase retired the collated index and something rebuilt it",
    );

    // And the index still answers, which is what the keys are for.
    assert_eq!(
        rows_of(
            &mut e,
            "SELECT id FROM issues WHERE project_id = 1 AND fingerprint = 'fp2'"
        ),
        vec!["[Int(2)]".to_string()],
        "the composite collated index still finds the row",
    );
    assert_eq!(
        rows_of(&mut e, "SELECT n FROM issues WHERE id = 1"),
        vec!["[BigInt(1)]".to_string()],
        "A's first write survived its own rebases",
    );
    assert_eq!(
        rows_of(&mut e, "SELECT n FROM issues WHERE id = 100"),
        vec!["[BigInt(1)]".to_string()],
        "and B's writes are there too",
    );
}
