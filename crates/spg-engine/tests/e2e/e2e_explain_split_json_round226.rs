//! v7.39 (round 226, EXPLAIN epic Phase 2) — predicate splitting and
//! PG-shaped FORMAT JSON. Live-PG18.4 differential (2026-07-19):
//!   Index Scan using p1_pkey on p1
//!     Index Cond: (id = 5)
//!     Filter: (v > 3)
//! — the indexed conjunct goes to Index Cond, the residual to Filter.
//! FORMAT JSON emits PG's nested node objects
//! (`[{"Plan": {"Node Type": …, "Plans": [...]}}]`) instead of the old
//! per-line `{"Plan Line": …}` fallback.

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
    e.execute("INSERT INTO p1 VALUES (1,2,'a'),(2,4,'b'),(5,10,'x5')")
        .unwrap();
    e
}

#[test]
fn index_cond_and_filter_split_like_pg() {
    let mut e = seeded();
    // The PK conjunct is what the executor pushes into the index; the rest
    // stays a Filter — exactly PG's two-line Index Scan shape.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (COSTS OFF) SELECT * FROM p1 WHERE id = 5 AND v > 3"
        ),
        vec![
            "Index Scan using p1_pkey on p1",
            "  Index Cond: (id = 5)",
            "  Filter: (v > 3)",
        ]
    );
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (COSTS OFF) SELECT * FROM p1 WHERE id = 5 AND s = 'x5'"
        ),
        vec![
            "Index Scan using p1_pkey on p1",
            "  Index Cond: (id = 5)",
            "  Filter: (s = 'x5')",
        ]
    );
    // A lone indexed predicate keeps a single Index Cond, no Filter line.
    assert_eq!(
        plan(&mut e, "EXPLAIN (COSTS OFF) SELECT * FROM p1 WHERE id = 5"),
        vec!["Index Scan using p1_pkey on p1", "  Index Cond: (id = 5)"]
    );
    // No indexable conjunct → Seq Scan with the whole AND chain as Filter,
    // rendered PG-style `((a) AND (b))`.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN (COSTS OFF) SELECT * FROM p1 WHERE v > 3 AND s = 'a'"
        ),
        vec!["Seq Scan on p1", "  Filter: ((v > 3) AND (s = 'a'))"]
    );
}

#[test]
fn format_json_emits_pg_node_objects() {
    let mut e = seeded();
    let out = plan(
        &mut e,
        "EXPLAIN (FORMAT JSON, COSTS OFF) SELECT * FROM p1 WHERE id = 5 AND v > 3",
    );
    assert_eq!(out.len(), 1, "JSON is one row");
    let j = &out[0];
    // v7.39 (round 228) — PG pretty-prints the array/Plan wrapper.
    assert!(j.starts_with("[\n  {\n    \"Plan\": {"), "PG array/Plan wrapper: {j}");
    assert!(j.contains("\"Node Type\": \"Index Scan\""), "{j}");
    assert!(j.contains("\"Index Name\": \"p1_pkey\""), "{j}");
    assert!(j.contains("\"Relation Name\": \"p1\""), "{j}");
    assert!(j.contains("\"Index Cond\": \"(id = 5)\""), "{j}");
    assert!(j.contains("\"Filter\": \"(v > 3)\""), "{j}");
    // COSTS OFF omits the cost keys.
    assert!(!j.contains("Total Cost"), "COSTS OFF drops costs: {j}");
}

#[test]
fn format_json_nests_children_under_plans() {
    let mut e = seeded();
    let j = &plan(
        &mut e,
        "EXPLAIN (FORMAT JSON, COSTS OFF) SELECT v, count(*) FROM p1 GROUP BY v",
    )[0];
    // PG spells a hash-grouped aggregate Node Type "Aggregate" + Strategy
    // "Hashed", with the scan nested under "Plans" as the Outer child.
    assert!(j.contains("\"Node Type\": \"Aggregate\""), "{j}");
    assert!(j.contains("\"Strategy\": \"Hashed\""), "{j}");
    assert!(j.contains("\"Group Key\": [\"v\"]"), "key list is an array: {j}");
    assert!(j.contains("\"Plans\": ["), "{j}");
    assert!(j.contains("\"Parent Relationship\": \"Outer\""), "{j}");
    assert!(j.contains("\"Node Type\": \"Seq Scan\""), "{j}");
}

#[test]
fn format_json_carries_costs_when_on() {
    let mut e = seeded();
    let j = &plan(&mut e, "EXPLAIN (FORMAT JSON) SELECT * FROM p1 WHERE v = 4")[0];
    assert!(j.contains("\"Startup Cost\": 0.00"), "{j}");
    assert!(j.contains("\"Total Cost\": "), "{j}");
    assert!(j.contains("\"Plan Rows\": "), "{j}");
    assert!(j.contains("\"Plan Width\": "), "{j}");
}
