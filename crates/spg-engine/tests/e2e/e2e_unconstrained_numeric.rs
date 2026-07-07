//! v7.38 (read01 sweep) — an unconstrained NUMERIC column (no precision/scale)
//! accepts a value at its own natural scale: float, string, or already-scaled
//! numeric, not just integers. A declared numeric(p,s) still rescales. Oracle
//! behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn unconstrained_numeric_accepts_any_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sn (n NUMERIC)").unwrap();
    // float, string, and int literals all land.
    e.execute("INSERT INTO sn VALUES (3.14)").unwrap();
    e.execute("INSERT INTO sn VALUES ('2.5')").unwrap();
    e.execute("INSERT INTO sn VALUES (7)").unwrap();
    match e.execute("SELECT count(*) FROM sn").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], Value::BigInt(3)),
        _ => panic!(),
    }
    // UPDATE to a differently-scaled numeric also works.
    e.execute("UPDATE sn SET n = 9.99 WHERE n = 7").unwrap();
    assert_eq!(one(&mut e, "SELECT n FROM sn WHERE n = 9.99"), Value::Numeric { scaled: 999, scale: 2 });
}

#[test]
fn declared_numeric_still_rescales() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sp (n NUMERIC(10,2))").unwrap();
    // Rounds to the declared scale (PG: 3.14159 → 3.14).
    e.execute("INSERT INTO sp VALUES (3.14159)").unwrap();
    assert_eq!(one(&mut e, "SELECT n FROM sp"), Value::Numeric { scaled: 314, scale: 2 });
}
