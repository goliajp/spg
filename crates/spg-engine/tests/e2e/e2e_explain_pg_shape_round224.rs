//! v7.39 (round 224, EXPLAIN epic Phase 0) — EXPLAIN renders PG's
//! text-tree shape: node vocabulary (Seq Scan on / Index Scan using /
//! Sort / Limit / Aggregate / HashAggregate / Hash Join / Append /
//! WindowAgg / Result / CTE Scan), `->` arrows at column 6*depth-4,
//! attribute lines (Filter: / Sort Key: / Group Key: / Hash Cond:)
//! indented 2 past the node. Indentation measured off live PG18.4
//! (2026-07-19 probe). The tree shows SPG's REAL execution decisions
//! (its own index heuristic, its own join strategy) in PG's grammar,
//! so tools that parse PG plans read SPG plans unchanged.

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
    e.execute("CREATE TABLE t1 (id int PRIMARY KEY, v int)").unwrap();
    e.execute("CREATE TABLE t2 (id int PRIMARY KEY, t1_id int)")
        .unwrap();
    e.execute("INSERT INTO t1 VALUES (1,2),(2,4)").unwrap();
    e
}

#[test]
fn scan_shapes() {
    let mut e = seeded();
    assert_eq!(plan(&mut e, "EXPLAIN SELECT * FROM t1"), vec!["Seq Scan on t1"]);
    // SPG's real decision: the PK index serves id=5 (PG's small-table
    // planner would seq-scan — the SHAPE grammar is what's aligned).
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT * FROM t1 WHERE id = 5"),
        vec!["Index Scan using t1_pkey on t1", "  Index Cond: (id = 5)"]
    );
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT * FROM t1 WHERE v = 10"),
        vec!["Seq Scan on t1", "  Filter: (v = 10)"]
    );
    assert_eq!(plan(&mut e, "EXPLAIN SELECT 1"), vec!["Result"]);
}

#[test]
fn sort_limit_nesting_matches_pg_indentation() {
    let mut e = seeded();
    // PG:  Limit
    //        ->  Sort
    //              Sort Key: v
    //              ->  Seq Scan on t1
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT * FROM t1 ORDER BY v LIMIT 3"),
        vec![
            "Limit",
            "  ->  Sort",
            "        Sort Key: v",
            "        ->  Seq Scan on t1",
        ]
    );
}

#[test]
fn aggregate_shapes() {
    let mut e = seeded();
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT count(*) FROM t1"),
        vec!["Aggregate", "  ->  Seq Scan on t1"]
    );
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT v, count(*) FROM t1 GROUP BY v"),
        vec![
            "HashAggregate",
            "  Group Key: v",
            "  ->  Seq Scan on t1",
        ]
    );
    // DISTINCT plans as a HashAggregate over the select list (PG shape).
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT DISTINCT v FROM t1"),
        vec![
            "HashAggregate",
            "  Group Key: v",
            "  ->  Seq Scan on t1",
        ]
    );
}

#[test]
fn hash_join_shape() {
    let mut e = seeded();
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN SELECT * FROM t1 JOIN t2 ON t2.t1_id = t1.id"
        ),
        vec![
            "Hash Join",
            "  Hash Cond: (t2.t1_id = t1.id)",
            "  ->  Seq Scan on t1",
            "  ->  Hash",
            "        ->  Seq Scan on t2",
        ]
    );
}

#[test]
fn append_and_window_shapes() {
    let mut e = seeded();
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN SELECT * FROM t1 UNION ALL SELECT * FROM t1"
        ),
        vec![
            "Append",
            "  ->  Seq Scan on t1",
            "  ->  Seq Scan on t1",
        ]
    );
    assert_eq!(
        plan(&mut e, "EXPLAIN SELECT sum(v) OVER (ORDER BY id) FROM t1"),
        vec!["WindowAgg", "  ->  Seq Scan on t1"]
    );
}

#[test]
fn cte_block_shape() {
    let mut e = seeded();
    // Materialized CTE: `CTE <name>` label (no arrow) + arrowed body,
    // hung under the CTE Scan root.
    assert_eq!(
        plan(
            &mut e,
            "EXPLAIN WITH w AS (SELECT * FROM t1) SELECT * FROM w WHERE id = 1"
        ),
        vec![
            "CTE Scan on w",
            "  Filter: (id = 1)",
            "  CTE w",
            "        ->  Seq Scan on t1",
        ]
    );
}
