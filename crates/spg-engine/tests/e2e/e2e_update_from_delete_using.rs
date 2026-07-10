//! UPDATE ... FROM + DELETE ... USING — PG's joined DML, lowered
//! onto the correlated-subquery machinery.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn update_from_joins_source_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tgt (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("CREATE TABLE src (id INT, w INT)").unwrap();
    e.execute("INSERT INTO tgt VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    e.execute("INSERT INTO src VALUES (1, 100), (3, 300)")
        .unwrap();
    // Matched rows take the source value; unmatched stay.
    e.execute("UPDATE tgt SET v = src.w FROM src WHERE tgt.id = src.id")
        .unwrap();
    let got = rows(&mut e, "SELECT v FROM tgt ORDER BY id");
    assert_eq!(as_i64(&got[0][0]), 100);
    assert_eq!(as_i64(&got[1][0]), 20);
    assert_eq!(as_i64(&got[2][0]), 300);
}

#[test]
fn update_from_with_alias_and_mixed_assignment() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t2 (id INT PRIMARY KEY, v INT, n INT)")
        .unwrap();
    e.execute("CREATE TABLE s2 (id INT, w INT)").unwrap();
    e.execute("INSERT INTO t2 VALUES (1, 10, 0)").unwrap();
    e.execute("INSERT INTO s2 VALUES (1, 7)").unwrap();
    // One assignment reads the source, one only the target —
    // only the former wraps into a subquery.
    e.execute("UPDATE t2 SET v = s.w * 2, n = 5 FROM s2 AS s WHERE t2.id = s.id")
        .unwrap();
    let got = rows(&mut e, "SELECT v, n FROM t2");
    assert_eq!(as_i64(&got[0][0]), 14);
    assert_eq!(as_i64(&got[0][1]), 5);
}

#[test]
fn delete_using_removes_matches_only() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dt (id INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE kill (id INT)").unwrap();
    e.execute("INSERT INTO dt VALUES (1), (2), (3)").unwrap();
    e.execute("INSERT INTO kill VALUES (1), (3)").unwrap();
    e.execute("DELETE FROM dt USING kill WHERE dt.id = kill.id")
        .unwrap();
    let got = rows(&mut e, "SELECT id FROM dt");
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 2);
}
