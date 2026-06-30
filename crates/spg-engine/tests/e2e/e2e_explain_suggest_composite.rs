//! v7.37.19 (19.21) — EXPLAIN (SUGGEST) detects composite-index
//! opportunities for AND-chained equality predicates on the same
//! table.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn plan_text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    let mut out = String::new();
    for row in &rows {
        if let Value::Text(s) = &row.values[0] {
            out.push_str(s);
            out.push('\n');
        }
    }
    out
}

#[test]
fn explain_suggest_composite_for_and_chain_of_equals() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE orders (id INT, status TEXT, region TEXT, amount BIGINT)")
        .unwrap();
    let plan = plan_text(
        &mut e,
        "EXPLAIN (SUGGEST) SELECT * FROM orders \
         WHERE status = 'paid' AND region = 'jp'",
    );
    assert!(
        plan.contains("SUGGEST: CREATE INDEX")
            && plan.contains("status")
            && plan.contains("region"),
        "expected composite suggestion mentioning both cols: {plan}"
    );
    // The composite suggestion lists both columns in a single
    // CREATE INDEX statement (rather than two single-col entries).
    let composite_line = plan
        .lines()
        .find(|l| {
            l.contains("SUGGEST: CREATE INDEX")
                && l.contains("status")
                && l.contains("region")
        })
        .expect("composite suggestion line");
    assert!(
        composite_line.contains("(status, region)") || composite_line.contains("(region, status)"),
        "composite line shape: {composite_line}"
    );
}

#[test]
fn explain_suggest_no_composite_for_single_predicate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE orders (id INT, status TEXT)").unwrap();
    let plan = plan_text(
        &mut e,
        "EXPLAIN (SUGGEST) SELECT * FROM orders WHERE status = 'paid'",
    );
    // Only the single-col suggestion fires.
    let suggest_count = plan
        .lines()
        .filter(|l| l.contains("SUGGEST: CREATE INDEX"))
        .count();
    assert_eq!(
        suggest_count, 1,
        "expected exactly one suggestion for a single-eq filter: {plan}"
    );
}

#[test]
fn explain_suggest_skips_composite_when_already_covered() {
    // SPG only populates extra_column_positions for UNIQUE
    // multi-column indices today (plain CREATE INDEX with N
    // columns currently indexes only the leading column), so
    // verify the cover-check works for the case the storage
    // model already supports.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE orders (id INT NOT NULL, status TEXT, region TEXT, UNIQUE (status, region))",
    )
    .unwrap();
    let plan = plan_text(
        &mut e,
        "EXPLAIN (SUGGEST) SELECT * FROM orders \
         WHERE status = 'paid' AND region = 'jp'",
    );
    let has_composite = plan.lines().any(|l| {
        l.contains("SUGGEST: CREATE INDEX")
            && l.contains("status")
            && l.contains("region")
            && (l.contains("(status, region)") || l.contains("(region, status)"))
    });
    assert!(
        !has_composite,
        "composite suggestion should be skipped (UNIQUE covers both): {plan}"
    );
}
