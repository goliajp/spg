//! v7.37.17 (17.6 siblings) — SELECT DISTINCT ON (exprs), the
//! latest-per-key shape (Django's .distinct('field')).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn latest_per_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ev (uid INT, seq INT)").unwrap();
    e.execute("INSERT INTO ev VALUES (1, 10), (1, 30), (2, 20), (2, 5), (3, 1)")
        .unwrap();
    // PG canonical shape: first row per uid, ordered seq DESC →
    // the latest event per user.
    let got = rows(
        &mut e,
        "SELECT DISTINCT ON (uid) uid, seq FROM ev ORDER BY uid, seq DESC",
    );
    assert_eq!(got.len(), 3);
    assert_eq!((as_i64(&got[0][0]), as_i64(&got[0][1])), (1, 30));
    assert_eq!((as_i64(&got[1][0]), as_i64(&got[1][1])), (2, 20));
    assert_eq!((as_i64(&got[2][0]), as_i64(&got[2][1])), (3, 1));
}

#[test]
fn multi_key_distinct_on() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (a INT, b INT, c INT)").unwrap();
    e.execute("INSERT INTO m VALUES (1, 1, 100), (1, 1, 200), (1, 2, 300)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT DISTINCT ON (a, b) a, b, c FROM m ORDER BY a, b, c",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][2]), 100);
    assert_eq!(as_i64(&got[1][2]), 300);
}

#[test]
fn plain_distinct_unaffected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (x INT)").unwrap();
    e.execute("INSERT INTO p VALUES (1), (1), (2)").unwrap();
    let got = rows(&mut e, "SELECT DISTINCT x FROM p ORDER BY x");
    assert_eq!(got.len(), 2);
}
