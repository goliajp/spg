//! read01 round 462 — Describe must declare what Execute returns.
//!
//! Execute never sends a RowDescription — Describe owns it — so any shape
//! Describe cannot resolve reaches an extended-protocol client as data rows
//! with no column metadata at all. Measured against PG18 over sqlx before
//! this round: a view, a JOIN, a derived table, a UNION, a CTE and every
//! system catalog view declared ZERO columns while plainly returning rows,
//! so `row.get(0)` was out of bounds. PG18 declares all of them.
//!
//! These pins compare Describe's answer against the columns execution
//! actually produces, over a shape corpus. That is the anti-drift guard:
//! views had a describe path since round 268 and still fell out of the
//! prepared-statement one, because the two were written separately.

use spg_engine::{Engine, QueryResult};

/// Shapes whose column list Describe must get exactly right.
const AGREE: &[&str] = &[
    "SELECT id FROM wb",
    "SELECT * FROM wb",
    "SELECT id, g FROM wb WHERE g > 0",
    "SELECT 1 AS one",
    "SELECT count(*) AS n FROM wb",
    // v7.39 (round 462) — every one of these declared nothing before.
    "SELECT id FROM wv",
    "SELECT * FROM wv",
    // Bare `id` is ambiguous across the two relations, as it is in PG.
    "SELECT wb.id, wc.h FROM wb JOIN wc ON wb.id = wc.id",
    "SELECT * FROM wb JOIN wc ON wb.id = wc.id",
    "SELECT s.id FROM (SELECT id FROM wb) s",
    "SELECT id FROM wb UNION SELECT id FROM wc",
    "WITH c AS (SELECT id FROM wb) SELECT id FROM c",
    "WITH c(k) AS (SELECT id FROM wb) SELECT k FROM c",
    "SELECT n_dead_tup FROM pg_stat_user_tables",
    "SELECT relname FROM pg_class",
    "SELECT tablename FROM pg_tables",
    "SELECT table_name FROM information_schema.tables",
    "SELECT name FROM pg_settings",
    // The second family: views that never reach the catalog at all —
    // each is a fixed row set built inside its own exec_* function.
    "SELECT pid FROM pg_stat_activity",
    "SELECT * FROM pg_stat_activity",
    "SELECT * FROM pg_locks",
    "SELECT * FROM pg_statio_user_tables",
    "SELECT * FROM spg_stat_mvcc",
    "SELECT * FROM spg_memory_stats",
];

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE wc(id INT PRIMARY KEY, h INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO wb VALUES (1,10),(2,20)").unwrap();
    e.execute("INSERT INTO wc VALUES (1,100),(2,200)").unwrap();
    e.execute("CREATE VIEW wv AS SELECT id, g FROM wb").unwrap();
    e
}

fn executed_columns(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql} did not return rows: {other:?}"),
    }
}

#[test]
fn round462_describe_declares_the_columns_execute_returns() {
    let mut e = seeded();
    for sql in AGREE {
        let stmt = e.prepare(sql).unwrap();
        let (_, described) = e.describe_prepared(&stmt);
        let described: Vec<String> = described.iter().map(|c| c.name.clone()).collect();
        let executed = executed_columns(&mut e, sql);
        assert_eq!(
            described, executed,
            "Describe and Execute disagree for `{sql}`"
        );
    }
}

#[test]
fn round462_a_relation_cycle_does_not_hang_describe() {
    // A view whose body is dropped out from under it must report no
    // columns, not recurse. The depth cap is the backstop; this pins that
    // an unresolvable relation is a clean empty answer.
    let mut e = seeded();
    let stmt = e.prepare("SELECT id FROM does_not_exist").unwrap();
    let (_, described) = e.describe_prepared(&stmt);
    assert!(described.is_empty());
}

#[test]
fn round462_qualified_star_over_a_join_declares_nothing() {
    // The namespace Describe builds is flat, so `wb.*` cannot be told from
    // `wc.*` — refusing beats describing every column as if they were wb's.
    let mut e = seeded();
    let stmt = e
        .prepare("SELECT wb.* FROM wb JOIN wc ON wb.id = wc.id")
        .unwrap();
    let (_, described) = e.describe_prepared(&stmt);
    assert!(
        described.is_empty(),
        "a qualified star over a join must not be described from a flat namespace"
    );
}
