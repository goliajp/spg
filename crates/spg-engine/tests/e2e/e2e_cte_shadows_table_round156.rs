//! v7.39 (read01 round 156) — a CTE may shadow a same-named real table on
//! the read path (PG scoping): the WITH name wins for the outer query,
//! later CTEs, subqueries and joins; the shadowing CTE's OWN body still
//! sees the real table (a non-recursive self-name is the table); a
//! RECURSIVE CTE's self-reference is the CTE itself. The real table's
//! data is untouched throughout. SPG used to reject every one of these
//! with "shadows an existing table; rename the CTE" (the write-path
//! entries still do — recorded residual for round 157).
//! Locked byte-identical against PG 18.4 (r156 live probes).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match r.values[0] {
                spg_storage::Value::BigInt(n) => n,
                spg_storage::Value::Int(n) => i64::from(n),
                ref other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE st156(x int)").unwrap();
    e.execute("INSERT INTO st156 VALUES (100),(200)").unwrap();
}

/// P1/P3/P4/P5 — the CTE wins for the outer query, sibling CTEs,
/// subqueries and (twice-referenced) joins.
#[test]
fn cte_shadows_table_in_read_positions() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        col(&mut e, "WITH st156 AS (SELECT 1 AS x) SELECT * FROM st156"),
        vec![1]
    );
    assert_eq!(
        one(
            &mut e,
            "WITH st156 AS (SELECT 1 AS x), u AS (SELECT x*10 AS y FROM st156) SELECT * FROM u",
        ),
        10
    );
    assert_eq!(
        one(
            &mut e,
            "WITH st156 AS (SELECT 1 AS x) SELECT (SELECT count(*) FROM st156)",
        ),
        1
    );
    assert_eq!(
        one(
            &mut e,
            "WITH st156 AS (SELECT 1 AS x) \
             SELECT a.x + b.x FROM st156 a JOIN st156 b ON a.x = b.x",
        ),
        2
    );
    // The real table is untouched.
    assert_eq!(one(&mut e, "SELECT count(*) FROM st156"), 2);
}

/// P2 — the shadowing CTE's own (non-recursive) body sees the REAL table.
#[test]
fn shadowing_body_sees_real_table() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        col(
            &mut e,
            "WITH st156 AS (SELECT x+1 AS x FROM st156) SELECT * FROM st156 ORDER BY x",
        ),
        vec![101, 201]
    );
}

/// P6 — a RECURSIVE same-named CTE's self-reference is the CTE.
#[test]
fn recursive_cte_shadows_table() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        one(
            &mut e,
            "WITH RECURSIVE st156(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM st156 WHERE n < 3) \
             SELECT count(*) FROM st156",
        ),
        3
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM st156"), 2);
}
