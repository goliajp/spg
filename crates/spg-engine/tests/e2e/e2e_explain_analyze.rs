//! v6.2.4 — EXPLAIN ANALYZE per-operator stats.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: &QueryResult) -> Vec<String> {
    match r {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .filter_map(|row| {
                if let Value::Text(s) = &row.values[0] {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn every_operator_reports_stats() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    for i in 0..10 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, 'n{i}')"))
            .unwrap();
    }
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    let lines = rows_of(&r);
    assert!(!lines.is_empty(), "EXPLAIN ANALYZE must emit ≥ 1 line");
    // v7.39 (round 227) — PG-shaped ANALYZE: node lines carry an
    // `(actual … rows=N.NN loops=1)` block (or none, where SPG cannot
    // genuinely derive the count); the trailing line is PG's
    // `Execution Time:`. Attribute lines carry no stats.
    assert!(
        lines[0].contains("(actual ") && lines[0].contains("loops=1"),
        "top node carries the measured block: {:?}",
        lines[0]
    );
    // This engine has no clock, so the measured block carries rows only
    // and the `Execution Time:` summary is (correctly) absent — SPG does
    // not invent a timing it never took.
    assert!(
        !lines[0].contains("actual time="),
        "no clock ⇒ no per-node time: {:?}",
        lines[0]
    );
}

#[test]
fn top_level_rows_match_result_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..7 {
        e.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    let lines = rows_of(&r);
    let top = &lines[0];
    // v7.39 (round 227) — PG shape: the top node reports the real result
    // count in the measured block.
    assert!(
        top.contains("rows=7.00 loops=1"),
        "top reports actual result rows; got {top:?}"
    );
    // No clock injected on this engine, so PG's `Execution Time:` summary
    // (which needs a real measurement) is correctly absent.
    assert!(
        !lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "no clock ⇒ no fabricated timing line: {lines:?}"
    );
}

#[test]
fn scan_reports_catalog_row_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT NOT NULL)").unwrap();
    for i in 0..40 {
        e.execute(&format!("INSERT INTO big VALUES ({i})")).unwrap();
    }
    let r = e
        .execute("EXPLAIN ANALYZE SELECT * FROM big WHERE id < 10")
        .unwrap();
    let lines = rows_of(&r);
    // v7.39 (round 227) — PG shape: the filtered top-level Seq Scan
    // reports its OUTPUT rows plus `Rows Removed by Filter` (40 scanned
    // − 10 emitted = 30), both genuinely derived.
    let from_line = lines
        .iter()
        .find(|l| l.contains("Seq Scan on big"))
        .expect("scan line present");
    assert!(
        from_line.contains("rows=10.00 loops=1"),
        "scan reports actual output rows; got {from_line:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("Rows Removed by Filter: 30")),
        "PG's removed-rows line: {lines:?}"
    );
}

#[test]
fn no_unknown_operator_in_top_level() {
    // Walk a handful of representative SQL shapes; assert the
    // top-level operator is one of the known labels (not "unknown"
    // or empty).
    // v7.39 (round 224) — PG-shaped node vocabulary.
    let known: [&str; 8] = [
        "Seq",
        "Index",
        "Result",
        "Aggregate",
        "HashAggregate",
        "WindowAgg",
        "Append",
        "CTE",
    ];
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..5 {
        e.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let queries = [
        "EXPLAIN ANALYZE SELECT * FROM t",
        "EXPLAIN ANALYZE SELECT count(*) FROM t",
        "EXPLAIN ANALYZE SELECT DISTINCT id FROM t",
        "EXPLAIN ANALYZE SELECT 1",
        "EXPLAIN ANALYZE SELECT * FROM t UNION SELECT * FROM t",
    ];
    for q in queries {
        let r = e.execute(q).unwrap();
        let lines = rows_of(&r);
        let top = &lines[0];
        let stripped = top.split_once(' ').map_or(top.as_str(), |(head, _)| head);
        assert!(
            known.iter().any(|k| stripped.starts_with(k)),
            "unknown top operator {stripped:?} for query {q:?}"
        );
    }
}

#[test]
fn scan_omits_cold_marker_when_no_cold_segments() {
    // A freshly-created table with only hot rows must NOT advertise
    // cold_tier=present (catalog cold_segment_count() == 0).
    let mut e = Engine::new();
    e.execute("CREATE TABLE warm (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO warm VALUES (1)").unwrap();
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM warm").unwrap();
    let lines = rows_of(&r);
    let from = lines
        .iter()
        .find(|l| l.contains("Seq Scan on warm"))
        .unwrap();
    // v7.39 (round 227) — PG shape: actual rows on the scan; the old
    // SPG-vocabulary cold-tier marker is gone from the node line.
    assert!(
        from.contains("rows=1.00 loops=1"),
        "scan line shows actual rows; got {from:?}"
    );
    assert!(
        !from.contains("cold_tier"),
        "no cold marker on the PG-shaped node line; got {from:?}"
    );
}

#[test]
fn execution_time_line_present_when_clock_is_set() {
    // v7.39 (round 227) — PG's `Execution Time:` replaced SPG's
    // `Total: … elapsed=…us`. It needs an injected clock to measure.
    let mut e = Engine::new().with_clock(|| 1_000_000);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    let lines = rows_of(&r);
    let total = lines
        .iter()
        .find(|l| l.starts_with("Execution Time: "))
        .expect("Execution Time line present");
    assert!(total.ends_with(" ms"), "PG unit suffix: {total:?}");
}
