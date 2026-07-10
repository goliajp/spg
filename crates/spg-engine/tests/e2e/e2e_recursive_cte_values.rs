//! Recursive CTE with a VALUES seed: WITH RECURSIVE t(n) AS
//! (VALUES(1) UNION ALL SELECT n+1 FROM t WHERE n<5) — the VALUES
//! seed heads the set-operation chain.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<i64>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Int(n) => i64::from(*n),
                    spg_storage::Value::BigInt(n) => *n,
                    other => panic!("expected int, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn values_seed_counter() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "WITH RECURSIVE t(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM t WHERE n<5) \
         SELECT sum(n) FROM t",
    );
    assert_eq!(got, vec![vec![15]]);
}

#[test]
fn values_seed_fibonacci() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "WITH RECURSIVE fib(a,b) AS (VALUES(0,1) UNION ALL SELECT b, a+b FROM fib WHERE b<20) \
         SELECT a FROM fib ORDER BY a",
    );
    assert_eq!(
        got,
        vec![
            vec![0],
            vec![1],
            vec![1],
            vec![2],
            vec![3],
            vec![5],
            vec![8],
            vec![13],
        ]
    );
}

#[test]
fn non_recursive_values_cte_still_works() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "WITH t(a,b) AS (VALUES (1,10),(2,20)) SELECT sum(a), sum(b) FROM t",
    );
    assert_eq!(got, vec![vec![3, 30]]);
}

#[test]
fn recursive_cte_wellformedness_guardrails() {
    // v7.38 (read01) — PG rejects ORDER BY / LIMIT anywhere in a recursive
    // query and a self-reference that appears more than once in a term.
    // Live-PG18.4-verified error surfaces; valid shapes still run.
    let mut e = Engine::new();
    for bad in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<5 ORDER BY n) SELECT * FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<5 LIMIT 3) SELECT * FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT a.n+1 FROM t a, t b WHERE a.n<3) SELECT * FROM t",
    ] {
        assert!(e.execute(bad).is_err(), "should reject: {bad}");
    }
    // Valid: plain recursion, outer ORDER BY, non-recursive CTE ORDER BY.
    e.execute("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<3) SELECT * FROM t ORDER BY n").unwrap();
    e.execute("WITH x AS (SELECT 1 AS n ORDER BY n) SELECT * FROM x")
        .unwrap();
}
