//! v7.39 (read01 round 158) — verification pin: a CTE shadowing a
//! same-named VIEW behaves exactly like shadowing a table (PG, r158
//! live probes): the CTE wins for the outer query and write-path
//! sources, the shadowing CTE's own non-recursive body still sees the
//! VIEW, and a RECURSIVE self-reference is the CTE. No engine change
//! was needed — the rounds-156/157 shadow machinery covers it (the CTE
//! temp coexists with the view and table resolution takes priority) —
//! this pin locks that semantic against regression.

use spg_engine::{Engine, QueryResult};

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

#[test]
fn cte_shadows_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE vt158(x int)").unwrap();
    e.execute("INSERT INTO vt158 VALUES (100),(200)").unwrap();
    e.execute("CREATE VIEW vv158 AS SELECT x FROM vt158")
        .unwrap();
    e.execute("CREATE TABLE vo158(id int)").unwrap();
    // The CTE wins for the outer query.
    assert_eq!(
        col(&mut e, "WITH vv158 AS (SELECT 1 AS x) SELECT * FROM vv158"),
        vec![1]
    );
    // The shadowing CTE's own body sees the VIEW.
    assert_eq!(
        col(
            &mut e,
            "WITH vv158 AS (SELECT x+1 AS x FROM vv158) SELECT * FROM vv158 ORDER BY x",
        ),
        vec![101, 201]
    );
    // Write path: the INSERT source reads the CTE.
    match e
        .execute("WITH vv158 AS (SELECT 5 AS x) INSERT INTO vo158 SELECT x FROM vv158")
        .unwrap()
    {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(col(&mut e, "SELECT id FROM vo158"), vec![5]);
    // A RECURSIVE self-reference is the CTE.
    assert_eq!(
        col(
            &mut e,
            "WITH RECURSIVE vv158(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM vv158 WHERE n < 3) \
             SELECT count(*) FROM vv158",
        ),
        vec![3]
    );
    // The view itself is untouched.
    assert_eq!(
        col(&mut e, "SELECT x FROM vv158 ORDER BY x"),
        vec![100, 200]
    );
}
