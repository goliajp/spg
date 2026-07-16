//! v7.39 (read01 round 153) — two small residual closures:
//! 1. `ANY / ALL (WITH … SELECT …)` — the last un-widened nested-WITH
//!    subquery position (round-151 sibling); read-only works, a
//!    data-modifying CTE gets the top-level 0A000 error, no side effect.
//! 2. Bare `RETURNING *` in a MERGE through a column-renamed view keeps
//!    PG's range-table order — source columns (bare names) first, then
//!    the view's columns under their VIEW names (round-152 residual).
//! Locked byte-identical against PG 18.4 (r153 live probes).

use spg_engine::{Engine, QueryResult};

fn pairs(e: &mut Engine, sql: &str) -> Vec<(i32, i32)> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match (&r.values[0], &r.values[1]) {
                (spg_storage::Value::Int(a), spg_storage::Value::Int(b)) => (*a, *b),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t153(id int, v int)").unwrap();
    e.execute("INSERT INTO t153 VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE s153(id int, v int)").unwrap();
    e.execute("INSERT INTO s153 VALUES (1,200)").unwrap();
}

#[test]
fn any_all_with_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, v FROM t153 WHERE id = ANY(WITH c AS (SELECT 1 AS x) SELECT x FROM c)",
        ),
        vec![(1, 10)]
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, v FROM t153 \
             WHERE id > ALL(WITH c AS (SELECT 0 AS x) SELECT x FROM c) ORDER BY id",
        ),
        vec![(1, 10), (2, 20)]
    );
    // A modifying CTE inside ANY gets the top-level rule, nothing runs.
    let m = match e.execute(
        "SELECT id FROM t153 \
         WHERE id = ANY(WITH d AS (DELETE FROM s153 RETURNING id) SELECT id FROM d)",
    ) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error, got {r:?}"),
    };
    assert!(
        m.contains("WITH clause containing a data-modifying statement must be at the top level"),
        "{m}"
    );
    assert_eq!(pairs(&mut e, "SELECT id, v FROM s153"), vec![(1, 200)]);
}

#[test]
fn merge_bare_star_returning_via_renamed_view() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW rv153(a, b) AS SELECT id, v FROM t153")
        .unwrap();
    let r = e
        .execute(
            "MERGE INTO rv153 USING s153 s ON rv153.a = s.id \
             WHEN MATCHED THEN UPDATE SET b = 99 \
             RETURNING *",
        )
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, ["id", "v", "a", "b"]);
            assert_eq!(rows.len(), 1);
            let vals: Vec<i32> = rows[0]
                .values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Int(n) => *n,
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(vals, [1, 200, 1, 99]);
        }
        other => panic!("{other:?}"),
    }
}
