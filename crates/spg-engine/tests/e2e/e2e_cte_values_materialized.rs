//! v7.37.17 (17.6 siblings) — CTE AS [NOT] MATERIALIZED hints +
//! VALUES as a CTE body.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn values_cte_body() {
    let mut e = Engine::new();
    // PG shape: WITH t(a) AS (VALUES (1), (2)) SELECT a FROM t.
    assert_eq!(
        ints(
            &mut e,
            "WITH t(a) AS (VALUES (1), (2), (3)) SELECT a FROM t \
             WHERE a > 1 ORDER BY a"
        ),
        [2, 3]
    );
}

#[test]
fn materialized_hints_absorbed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (x INT)").unwrap();
    e.execute("INSERT INTO m VALUES (1), (2)").unwrap();
    assert_eq!(
        ints(
            &mut e,
            "WITH t AS MATERIALIZED (SELECT x FROM m) SELECT x FROM t ORDER BY x"
        ),
        [1, 2]
    );
    assert_eq!(
        ints(
            &mut e,
            "WITH t AS NOT MATERIALIZED (SELECT x FROM m WHERE x > 1) \
             SELECT x FROM t"
        ),
        [2]
    );
}
