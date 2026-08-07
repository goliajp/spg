//! v7.39 (round 227, EXPLAIN epic Phase 3) — EXPLAIN ANALYZE in PG's
//! shape. Live-PG18.4 differential (2026-07-19):
//!   Seq Scan on p1 (actual rows=1.00 loops=1)
//!     Filter: (v = 4)
//!     Rows Removed by Filter: 199
//!   Execution Time: 0.007 ms
//! SPG emits the same grammar with GENUINELY MEASURED numbers only:
//! the top node's elapsed IS the query elapsed, its rows ARE the result
//! count, an unfiltered Seq Scan emits exactly the live-row count, and a
//! filtered top-level scan's removed-rows is (live − result). Nodes whose
//! actual count SPG cannot derive from a real measurement carry NO block
//! rather than a fabricated one, and `Planning Time:` is omitted entirely
//! because SPG's planning is not separately instrumented — documented
//! divergences, not silent guesses.

use spg_engine::{Engine, QueryResult};

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p1 (id int PRIMARY KEY, v int, s text)")
        .unwrap();
    e.execute("INSERT INTO p1 VALUES (1,2,'a'),(2,4,'b'),(3,6,'c'),(4,4,'d')")
        .unwrap();
    e
}

#[test]
fn analyze_scan_shape_matches_pg() {
    let mut e = seeded();
    // Top-level filtered Seq Scan: output rows + PG's removed-rows line,
    // both genuinely derived (4 live − 2 emitted = 2 removed).
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT * FROM p1 WHERE v = 4"
        ),
        vec![
            "Seq Scan on p1 (actual rows=2.00 loops=1)",
            "  Filter: (v = 4)",
            "  Rows Removed by Filter: 2",
        ]
    );
    // Index Scan reports its real output count.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT * FROM p1 WHERE id = 2"
        ),
        vec![
            "Index Scan using p1_pkey on p1 (actual rows=1.00 loops=1)",
            "  Index Cond: (id = 2)",
        ]
    );
}

#[test]
fn analyze_annotates_nested_nodes() {
    let mut e = seeded();
    // Aggregate over an unfiltered scan: the top reports the group count,
    // the leaf scan the live-row count it genuinely reads.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) \
             SELECT v, count(*) FROM p1 GROUP BY v"
        ),
        vec![
            "HashAggregate (actual rows=3.00 loops=1)",
            "  Group Key: v",
            "  ->  Seq Scan on p1 (actual rows=4.00 loops=1)",
        ]
    );
}

#[test]
fn summary_and_timing_options_follow_pg() {
    let mut e = Engine::new().with_clock(|| 1_000_000);
    e.execute("CREATE TABLE t (id int)").unwrap();
    e.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    // TIMING OFF keeps PG's summary (it only drops per-node times).
    let lines = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT * FROM t",
    );
    assert!(
        !lines[0].contains("actual time="),
        "no node time: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "TIMING OFF keeps the summary (PG): {lines:?}"
    );
    // SUMMARY OFF drops it.
    let lines = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT * FROM t",
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "SUMMARY OFF drops it: {lines:?}"
    );
    // TIMING ON puts the measured time on the top node only.
    let lines = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF) SELECT * FROM t",
    );
    assert!(
        lines[0].contains("actual time=") && lines[0].contains("loops=1"),
        "top node carries the measured time: {lines:?}"
    );
}

#[test]
fn no_clock_means_no_invented_timing() {
    // Without an injected clock SPG measured nothing — it must not print
    // a timing it never took.
    let mut e = seeded();
    let lines = plan(&mut e, "EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM p1");
    assert!(!lines[0].contains("actual time="), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "{lines:?}"
    );
    // …but the genuinely-known row count is still reported.
    assert!(lines[0].contains("rows=4.00 loops=1"), "{lines:?}");
}

#[test]
fn planning_time_is_omitted_not_faked() {
    // PG prints `Planning Time:` too; SPG's planning is not separately
    // instrumented, so the line is omitted rather than invented.
    let mut e = Engine::new().with_clock(|| 1_000_000);
    e.execute("CREATE TABLE t (id int)").unwrap();
    let lines = plan(&mut e, "EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM t");
    assert!(
        !lines.iter().any(|l| l.starts_with("Planning Time: ")),
        "no fabricated planning time: {lines:?}"
    );
}
