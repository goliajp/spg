//! v7.38 (read01, P4.21 side-discovery) — PG's internal integer type names
//! int2 / int4 / int8 are accepted as column types (pg_dump and PG schemas
//! use them interchangeably with smallint / int / bigint). The cast path
//! already accepted them; only the column grammar rejected them.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn int2_int4_int8_are_column_types() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a int2, b int4, c int8)").unwrap();
    e.execute("INSERT INTO t VALUES (5, 100000, 9999999999)").unwrap();
    // Each maps to the right width.
    assert_eq!(one(&mut e, "SELECT a FROM t"), spg_storage::Value::SmallInt(5));
    assert_eq!(one(&mut e, "SELECT b FROM t"), spg_storage::Value::Int(100000));
    assert_eq!(
        one(&mut e, "SELECT c FROM t"),
        spg_storage::Value::BigInt(9999999999)
    );
    // Interchangeable with the standard names.
    e.execute("CREATE TABLE u(a smallint, b int, c bigint)").unwrap();
    e.execute("INSERT INTO u SELECT a, b, c FROM t").unwrap();
    assert_eq!(one(&mut e, "SELECT c FROM u"), spg_storage::Value::BigInt(9999999999));
}
