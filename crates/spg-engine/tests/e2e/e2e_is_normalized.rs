//! v7.38 (read01 sweep) — SQL:2016 `x IS [NOT] [form] NORMALIZED` predicate.
//! Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Bool(v) => *v,
            v => panic!("expected bool, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn is_normalized_predicate() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT 'abc' IS NORMALIZED"));
    // 'café' with a precomposed é is NFC-normalized but not NFD.
    assert!(b(&mut e, "SELECT 'café' IS NFC NORMALIZED"));
    assert!(!b(&mut e, "SELECT 'café' IS NFD NORMALIZED"));
    assert!(!b(&mut e, "SELECT 'abc' IS NOT NORMALIZED"));
    assert!(b(&mut e, "SELECT 'café' IS NOT NFD NORMALIZED"));
}

#[test]
fn normalized_still_usable_as_identifier() {
    // The predicate keyword must not steal "normalized" as a column name.
    let mut e = Engine::new();
    match e
        .execute("SELECT normalized FROM (SELECT 1 AS normalized) t")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], spg_storage::Value::Int(1)),
        _ => panic!(),
    }
}
