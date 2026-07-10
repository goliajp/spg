//! v7.38 (read01 sweep) — INSERT with no column list may supply fewer values
//! than the table has columns; trailing columns take DEFAULT / NULL. Oracle
//! behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values.clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn insert_fewer_values_fills_trailing_defaults() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fv (a INT, b INT, c INT DEFAULT 99)")
        .unwrap();
    // Two of three columns: c takes its default.
    e.execute("INSERT INTO fv VALUES (1, 2)").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a,b,c FROM fv WHERE a=1"),
        vec![Value::Int(1), Value::Int(2), Value::Int(99)]
    );
    // One of three: b (no default) becomes NULL, c its default.
    e.execute("INSERT INTO fv VALUES (10)").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a,b,c FROM fv WHERE a=10"),
        vec![Value::Int(10), Value::Null, Value::Int(99)]
    );
    // More values than columns is still rejected.
    assert!(e.execute("INSERT INTO fv VALUES (1,2,3,4)").is_err());
    // An explicit column list must match its value count exactly.
    assert!(e.execute("INSERT INTO fv (a,b,c) VALUES (8,9)").is_err());
}

#[test]
fn insert_fewer_values_computes_trailing_generated_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (id INT, x INT, gen INT GENERATED ALWAYS AS (x*2) STORED)")
        .unwrap();
    // Short row omits the generated column, which still computes.
    e.execute("INSERT INTO g VALUES (1, 5)").unwrap();
    assert_eq!(
        row(&mut e, "SELECT id,x,gen FROM g WHERE id=1"),
        vec![Value::Int(1), Value::Int(5), Value::Int(10)]
    );
}
