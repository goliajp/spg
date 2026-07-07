//! v7.38 (read01 P6.49) — CREATE TABLE AS SELECT (CTAS) creates a plain table
//! from a query result: columns/types inferred, rows materialised, and (unlike
//! a MATERIALIZED VIEW) it is a normal table (INSERT works, REFRESH does not).
//! Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.iter().map(|r| r.values.clone()).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn ctas_materialises_query_result() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (id INT, x INT)").unwrap();
    e.execute("INSERT INTO src VALUES (1, 10), (2, 20)").unwrap();
    e.execute("CREATE TABLE dst AS SELECT id, x * 2 AS dbl FROM src")
        .unwrap();
    let got = rows(&mut e, "SELECT id, dbl FROM dst ORDER BY id");
    assert_eq!(got[0], vec![spg_storage::Value::Int(1), spg_storage::Value::Int(20)]);
    assert_eq!(got[1], vec![spg_storage::Value::Int(2), spg_storage::Value::Int(40)]);
}

#[test]
fn ctas_result_is_a_plain_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (id INT)").unwrap();
    e.execute("INSERT INTO src VALUES (1)").unwrap();
    e.execute("CREATE TABLE dst AS SELECT id FROM src").unwrap();
    // A plain table accepts INSERT ...
    e.execute("INSERT INTO dst VALUES (2)").unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM dst")[0][0], spg_storage::Value::BigInt(2));
    // ... but is not a materialized view.
    assert!(e.execute("REFRESH MATERIALIZED VIEW dst").is_err());
}

#[test]
fn ctas_with_no_data_creates_empty_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (id INT)").unwrap();
    e.execute("INSERT INTO src VALUES (1), (2)").unwrap();
    e.execute("CREATE TABLE dst AS SELECT id FROM src WITH NO DATA")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM dst")[0][0], spg_storage::Value::BigInt(0));
}
