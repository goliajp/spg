//! v7.37.17 (17.6 siblings) — plain derived tables:
//! FROM ( SELECT … ) alias, riding the lateral_subquery channel.

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
fn basic_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let got = rows(
        &mut e,
        "SELECT x FROM (SELECT x FROM t WHERE x > 1) sub ORDER BY x",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[1][0]), 3);
}

#[test]
fn union_inside_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT)").unwrap();
    e.execute("INSERT INTO u VALUES (1)").unwrap();
    // The #328 shape that exposed the gap.
    let got = rows(
        &mut e,
        "SELECT x FROM (SELECT x FROM u UNION ALL SELECT x + 10 FROM u) sub \
         ORDER BY x",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[1][0]), 11);
}

#[test]
fn column_alias_list_renames() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ca (x INT, y INT)").unwrap();
    e.execute("INSERT INTO ca VALUES (1, 10), (2, 20)").unwrap();
    // AS t(a, b) renames positionally; the outer query addresses
    // the renamed columns.
    let got = rows(
        &mut e,
        "SELECT a, b FROM (SELECT x, y FROM ca) t(a, b) WHERE a = 2",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[0][1]), 20);
}

#[test]
fn derived_table_joins_a_real_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE facts (k INT, v TEXT)").unwrap();
    e.execute("INSERT INTO facts VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    e.execute("CREATE TABLE keys (k INT)").unwrap();
    e.execute("INSERT INTO keys VALUES (2)").unwrap();
    let got = rows(
        &mut e,
        "SELECT f.v FROM (SELECT k FROM keys) sub \
         JOIN facts f ON f.k = sub.k",
    );
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "two"));
}

#[test]
fn aggregate_over_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (x INT)").unwrap();
    e.execute("INSERT INTO a VALUES (1), (2), (3), (4)").unwrap();
    let got = rows(
        &mut e,
        "SELECT COUNT(*), SUM(x) FROM (SELECT x FROM a WHERE x > 1) big",
    );
    assert_eq!(as_i64(&got[0][0]), 3);
    assert_eq!(as_i64(&got[0][1]), 9);
}
