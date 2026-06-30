//! v7.37.7 C.1 — `EXPLAIN (COSTS OFF) <SELECT>` SQL surface.
//!
//! PG-standard option for diff-friendly EXPLAIN output. Strips the
//! wall-clock `elapsed=…us` annotation from the Total line so two
//! runs of the same plan emit byte-equal text. The base
//! `EXPLAIN ANALYZE` form keeps emitting the annotation.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn explain_lines(r: &QueryResult) -> Vec<String> {
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

fn build_engine() -> Engine {
    let mut e = Engine::new().with_clock(|| 0);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..5 {
        e.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    e
}

#[test]
fn explain_costs_off_strips_elapsed_from_total_line() {
    let mut e = build_engine();
    let r = e
        .execute("EXPLAIN (COSTS OFF) SELECT * FROM t")
        .expect("EXPLAIN (COSTS OFF) parses + executes");
    let lines = explain_lines(&r);
    assert!(!lines.is_empty(), "expected at least one plan line");
    let total = lines.iter().rev().find(|l| l.starts_with("Total:"));
    if let Some(total) = total {
        assert!(
            !total.contains("elapsed="),
            "EXPLAIN (COSTS OFF) Total line must not carry elapsed=…us, got: {total}"
        );
    }
}

#[test]
fn explain_costs_on_keeps_default_behaviour() {
    let mut e = build_engine();
    // Plain EXPLAIN (no COSTS option) still parses and executes.
    let r = e
        .execute("EXPLAIN SELECT * FROM t")
        .expect("EXPLAIN parses");
    let lines = explain_lines(&r);
    assert!(!lines.is_empty(), "expected at least one plan line");
}

#[test]
fn explain_costs_explicit_on_is_no_op() {
    let mut e = build_engine();
    // `EXPLAIN (COSTS ON)` is the PG default — must parse and behave
    // like plain EXPLAIN.
    let r = e
        .execute("EXPLAIN (COSTS ON) SELECT * FROM t")
        .expect("EXPLAIN (COSTS ON) parses");
    let lines = explain_lines(&r);
    assert!(!lines.is_empty());
}

#[test]
fn explain_combines_costs_with_suggest() {
    let mut e = build_engine();
    let r = e
        .execute("EXPLAIN (SUGGEST, COSTS OFF) SELECT * FROM t WHERE id = 3")
        .expect("EXPLAIN (SUGGEST, COSTS OFF) parses (comma-separated options)");
    let lines = explain_lines(&r);
    assert!(!lines.is_empty(), "expected plan lines");
}

#[test]
fn explain_unknown_option_errors() {
    // v7.37.22 (22.7) widened EXPLAIN to accept BUFFERS / TIMING /
    // SETTINGS / WAL / VERBOSE / FORMAT / SUMMARY. The "unknown
    // option" parse error now surfaces only on truly unrecognised
    // keywords. Update the test to assert that a fabricated
    // keyword still raises the option-list error.
    let mut e = build_engine();
    let err = e
        .execute("EXPLAIN (NONSENSE_KEYWORD) SELECT * FROM t")
        .expect_err("unknown option must surface a parse error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("EXPLAIN option") || msg.contains("NONSENSE_KEYWORD"),
        "error should mention the unsupported option, got: {msg}"
    );
}
