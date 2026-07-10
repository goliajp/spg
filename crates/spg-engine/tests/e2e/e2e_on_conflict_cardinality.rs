//! v7.38 (read01 sweep) — INSERT ... ON CONFLICT DO UPDATE refuses to affect
//! the same row twice within one command (PG cardinality violation), whether
//! the duplicate conflict key already exists in the table or only within the
//! batch. DO NOTHING still tolerates duplicates. Oracle from live PG 18.4.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn do_update_cannot_affect_row_twice() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE up (k INT PRIMARY KEY, c INT)")
        .unwrap();
    e.execute("INSERT INTO up VALUES (1, 1)").unwrap();
    // Duplicate key already in the table → cardinality violation, nothing applied.
    assert!(
        e.execute("INSERT INTO up VALUES (1,1),(1,1) ON CONFLICT (k) DO UPDATE SET c = up.c + 1")
            .is_err()
    );
    assert_eq!(scalar(&mut e, "SELECT c FROM up WHERE k=1"), Value::Int(1));

    // Duplicate key only within the batch (not yet in the table) → also errors.
    e.execute("CREATE TABLE af (k INT PRIMARY KEY, c INT)")
        .unwrap();
    assert!(
        e.execute("INSERT INTO af VALUES (2,1),(2,2) ON CONFLICT (k) DO UPDATE SET c = EXCLUDED.c")
            .is_err()
    );
}

#[test]
fn do_nothing_and_distinct_keys_still_work() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE af (k INT PRIMARY KEY, c INT)")
        .unwrap();
    // DO NOTHING tolerates a duplicate — the second is skipped.
    e.execute("INSERT INTO af VALUES (3,1),(3,2) ON CONFLICT (k) DO NOTHING")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT c FROM af WHERE k=3"), Value::Int(1));
    // Distinct keys with DO UPDATE are fine.
    e.execute("INSERT INTO af VALUES (10,1),(11,2) ON CONFLICT (k) DO UPDATE SET c = EXCLUDED.c")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM af"), Value::BigInt(3));
}
