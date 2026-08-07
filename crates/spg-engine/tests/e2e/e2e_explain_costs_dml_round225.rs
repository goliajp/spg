//! v7.39 (round 225, EXPLAIN epic Phase 1) — cost annotations + EXPLAIN
//! DML. Bare `EXPLAIN` now suffixes every plan node with PG's
//! `(cost=A..B rows=N width=W)` — the FORMAT is PG's (two decimals,
//! double-space before the paren), the NUMBERS are SPG's own estimates
//! (real live-row counts, fixed per-type widths, simple selectivity) —
//! and `EXPLAIN INSERT/UPDATE/DELETE` parses and renders PG's
//! `<Verb> on <table>` root over the source plan. EXPLAIN ANALYZE on
//! DML is rejected honestly (it would execute the write).

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
    e.execute("CREATE TABLE t1 (id int PRIMARY KEY, v int)")
        .unwrap();
    e.execute("INSERT INTO t1 VALUES (1,2),(2,4),(3,6)")
        .unwrap();
    e
}

#[test]
fn bare_explain_carries_pg_cost_format() {
    let mut e = seeded();
    // Line-exact: rows = the real live count (3), width from the schema
    // (int4 + int4 = 8); the cost figures are SPG's own model.
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT * FROM t1"),
        vec!["Seq Scan on t1  (cost=0.00..1.03 rows=3 width=8)"]
    );
    // Filter halves nothing away in the format — Filter attr keeps its
    // own line, no cost on attribute lines (PG shape).
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT * FROM t1 WHERE v = 10"),
        vec![
            "Seq Scan on t1  (cost=0.00..1.04 rows=1 width=8)",
            "  Filter: (v = 10)",
        ]
    );
    // COSTS OFF strips the suffix entirely.
    assert_eq!(
        plan(&mut e, "EXPLAIN (COSTS OFF) SELECT * FROM t1"),
        vec!["Seq Scan on t1"]
    );
}

#[test]
fn nested_nodes_all_costed() {
    let mut e = seeded();
    let lines = plan(&mut e, "EXPLAIN SELECT * FROM t1 ORDER BY v LIMIT 2");
    // Every NODE line carries a cost suffix; attribute lines don't.
    for l in &lines {
        let t = l.trim_start();
        let is_node =
            t.starts_with("Limit") || t.starts_with("->  Sort") || t.starts_with("->  Seq Scan");
        if is_node {
            assert!(l.contains("(cost="), "node line missing cost: {l}");
            assert!(l.contains("width="), "node line missing width: {l}");
        } else {
            assert!(!l.contains("(cost="), "attr line must not carry cost: {l}");
        }
    }
    // Limit caps the row estimate at the literal.
    assert!(
        lines[0].contains("rows=2"),
        "Limit rows capped at 2: {}",
        lines[0]
    );
}

#[test]
fn explain_dml_shapes() {
    let mut e = seeded();
    assert_eq!(
        plan(&mut e, "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (9, 1)"),
        vec!["Insert on t1", "  ->  Result"]
    );
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (COSTS OFF) UPDATE t1 SET v = 0 WHERE id = 1"
        ),
        vec![
            "Update on t1",
            "  ->  Index Scan using t1_pkey on t1",
            "        Index Cond: (id = 1)",
        ]
    );
    assert_eq!(
        plan(&mut e, "EXPLAIN (COSTS OFF) DELETE FROM t1 WHERE id = 1"),
        vec![
            "Delete on t1",
            "  ->  Index Scan using t1_pkey on t1",
            "        Index Cond: (id = 1)",
        ]
    );
    // INSERT … SELECT plans its source.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (COSTS OFF) INSERT INTO t1 SELECT id+10, v FROM t1"
        ),
        vec!["Insert on t1", "  ->  Seq Scan on t1"]
    );
    // The DML root carries a cost suffix under bare EXPLAIN.
    let lines = plan(&mut e, "EXPLAIN DELETE FROM t1 WHERE id = 1");
    assert!(
        lines[0].starts_with("Delete on t1  (cost="),
        "DML root costed: {}",
        lines[0]
    );
    // EXPLAIN must NOT have executed anything: still 3 rows.
    assert_eq!(
        plan(&mut e, "EXPLAIN (COSTS OFF) SELECT * FROM t1"),
        vec!["Seq Scan on t1"]
    );
    let QueryResult::Rows { rows, .. } = e.execute("SELECT count(*) FROM t1").unwrap() else {
        panic!()
    };
    assert_eq!(format!("{:?}", rows[0].values[0]), "BigInt(3)");
}

#[test]
fn explain_analyze_dml_executes() {
    // r225 pinned the REFUSAL here — "it would execute the write" — which
    // read like a policy but was a structural fact: the explain path took
    // `&self`. PG's ANALYZE really runs the statement and does not roll
    // back, so round 286 gave it a `&mut self` sibling and this pin now
    // asserts the measured PG behaviour instead.
    let mut e = seeded();
    let lines = plan(&mut e, "EXPLAIN ANALYZE DELETE FROM t1 WHERE id = 1");
    assert!(lines[0].starts_with("Delete on t1"), "{:?}", lines[0]);
    let QueryResult::Rows { rows, .. } = e.execute("SELECT count(*) FROM t1").unwrap() else {
        panic!()
    };
    assert_eq!(format!("{:?}", rows[0].values[0]), "BigInt(2)");
}
