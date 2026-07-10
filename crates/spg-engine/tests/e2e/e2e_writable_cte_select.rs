//! WITH d AS (DML ... RETURNING ...) SELECT ... — writable-CTE
//! outer SELECT routed through the transactional temp machinery.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn delete_returning_feeds_outer_select() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE q (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO q VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    // The moved rows surface through the CTE; the delete lands.
    let got = rows(
        &mut e,
        "WITH moved AS (DELETE FROM q WHERE v > 15 RETURNING id, v) \
         SELECT id, v FROM moved ORDER BY id",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[1][1]), 30);
    let left = rows(&mut e, "SELECT id FROM q");
    assert_eq!(left.len(), 1);
    assert_eq!(as_i64(&left[0][0]), 1);
}

#[test]
fn update_returning_aggregated_by_outer() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u2 (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO u2 VALUES (1, 1), (2, 2)").unwrap();
    let got = rows(
        &mut e,
        "WITH bumped AS (UPDATE u2 SET v = v * 10 RETURNING v) \
         SELECT sum(v) FROM bumped",
    );
    let total = match &got[0][0] {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        spg_storage::Value::Float(f) => *f as i64,
        other => panic!("expected number, got {other:?}"),
    };
    assert_eq!(total, 30);
    // Writes landed on the base table.
    let base = rows(&mut e, "SELECT v FROM u2 ORDER BY id");
    assert_eq!(as_i64(&base[1][0]), 20);
}
