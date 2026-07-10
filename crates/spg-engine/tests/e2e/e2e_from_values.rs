//! v7.37.17 (17.6 siblings) — FROM (VALUES ...) AS t(cols), lowered
//! onto the derived-table channel.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn values_with_column_aliases() {
    let mut e = Engine::new();
    // The c1 gap-probe shape.
    let got = rows(
        &mut e,
        "SELECT id, name FROM (VALUES (1, 'a'), (2, 'b')) AS v(id, name) \
         ORDER BY id",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert!(matches!(&got[0][1], spg_storage::Value::Text(s) if s == "a"));
    assert_eq!(as_i64(&got[1][0]), 2);
    assert!(matches!(&got[1][1], spg_storage::Value::Text(s) if s == "b"));
}

#[test]
fn default_column_names_are_pg_columnn() {
    let mut e = Engine::new();
    // Without a column list, PG names the columns column1..columnN.
    let got = rows(&mut e, "SELECT column2 FROM (VALUES (1, 'x')) t");
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "x"));
}

#[test]
fn values_join_and_aggregate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE facts (k INT, v TEXT)").unwrap();
    e.execute("INSERT INTO facts VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .unwrap();
    // VALUES as a filter list joined against a real table.
    let got = rows(
        &mut e,
        "SELECT f.v FROM (VALUES (1), (3)) want(k) \
         JOIN facts f ON f.k = want.k ORDER BY f.k",
    );
    assert_eq!(got.len(), 2);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "one"));
    assert!(matches!(&got[1][0], spg_storage::Value::Text(s) if s == "three"));
    // Aggregate over a VALUES list.
    let got = rows(&mut e, "SELECT SUM(x) FROM (VALUES (1), (2), (3)) t(x)");
    assert_eq!(as_i64(&got[0][0]), 6);
}
