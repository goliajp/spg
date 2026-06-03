#![allow(
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]

//! v6.2.4 — EXPLAIN ANALYZE per-operator stats.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: &QueryResult) -> Vec<String> {
    match r {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .filter_map(|row| {
                if let Value::Text(s) = &row.values[0] {
                    Some(s.clone())
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
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    for i in 0..10 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, 'n{i}')")).unwrap();
    }
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    let lines = rows_of(&r);
    assert!(!lines.is_empty(), "EXPLAIN ANALYZE must emit ≥ 1 line");
    // Every operator line carries either `(rows=N)`, `(rows=—)`,
    // or `(rows_scanned…)`. The trailing Total line carries
    // `Total: rows=…`.
    for line in &lines {
        let ok = line.contains("(rows=")
            || line.contains("(rows_scanned")
            || line.starts_with("Total: rows=");
        assert!(ok, "no stats annotation on line: {line:?}");
    }
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
    assert!(top.contains("(rows=7)"), "top reports result rows; got {top:?}");
    let total = lines.iter().find(|l| l.starts_with("Total: rows=")).unwrap();
    assert!(total.contains("rows=7"));
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
    // The "From: big [full scan]" line should report
    // `(rows_scanned=40)` — every catalog row was a candidate.
    let from_line = lines
        .iter()
        .find(|l| l.contains("From: big"))
        .expect("From line present");
    assert!(
        from_line.contains("rows_scanned=40"),
        "From line reports rows_scanned; got {from_line:?}"
    );
}

#[test]
fn no_unknown_operator_in_top_level() {
    // Walk a handful of representative SQL shapes; assert the
    // top-level operator is one of the known labels (not "unknown"
    // or empty).
    let known: [&str; 7] = [
        "TableScan",
        "Result",
        "Aggregate",
        "Distinct",
        "WindowAgg",
        "UnionScan",
        "CTEScan",
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
        let stripped = top
            .split_once(' ')
            .map_or(top.as_str(), |(head, _)| head);
        assert!(
            known.iter().any(|k| stripped.starts_with(k)),
            "unknown top operator {stripped:?} for query {q:?}"
        );
    }
}

#[test]
fn trailing_total_line_has_elapsed_when_clock_is_set() {
    // Engine without a clock injected → elapsed is omitted. With
    // a clock, the Total line should include `elapsed=…us`.
    let mut e = Engine::new().with_clock(|| 1_000_000);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = e.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    let lines = rows_of(&r);
    let total = lines.iter().find(|l| l.starts_with("Total: ")).unwrap();
    assert!(total.contains("elapsed="), "got: {total:?}");
}
