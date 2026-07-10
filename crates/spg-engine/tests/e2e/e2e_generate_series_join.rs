//! generate_series in a FROM list next to real tables — the
//! join-position materialise path.

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
fn cross_join_with_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (k INT)").unwrap();
    e.execute("INSERT INTO t VALUES (7), (8)").unwrap();
    let got = rows(
        &mut e,
        "SELECT k, g FROM t, generate_series(1, 2) AS s(g) ORDER BY k, g",
    );
    assert_eq!(got.len(), 4);
    assert_eq!(as_i64(&got[0][0]), 7);
    assert_eq!(as_i64(&got[0][1]), 1);
    assert_eq!(as_i64(&got[3][0]), 8);
    assert_eq!(as_i64(&got[3][1]), 2);
}

#[test]
fn join_position_with_ordinality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE j2 (k INT)").unwrap();
    e.execute("INSERT INTO j2 VALUES (1)").unwrap();
    let got = rows(
        &mut e,
        "SELECT s.i FROM j2, generate_series(10, 30, 10) \
         WITH ORDINALITY AS s(v, i) ORDER BY s.i",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[2][0]), 3);
}

#[test]
fn series_join_series() {
    let mut e = Engine::new();
    // Two SRFs in one FROM list — 2 × 3 cross product.
    let got = rows(
        &mut e,
        "SELECT a.x, b.y FROM generate_series(1, 2) AS a(x), \
         generate_series(1, 3) AS b(y) ORDER BY a.x, b.y",
    );
    assert_eq!(got.len(), 6);
    assert_eq!(as_i64(&got[5][0]), 2);
    assert_eq!(as_i64(&got[5][1]), 3);
}
