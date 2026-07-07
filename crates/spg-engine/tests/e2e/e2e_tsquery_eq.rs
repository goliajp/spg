//! v7.38 (read01 P6.31) — tsquery equality (= / <>) by structural compare.
//! PG does NOT normalise operand order, so `a & b` <> `b & a`. Oracle
//! behaviour from live PG 18.4.

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
fn tsquery_equality_is_structural() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT ('a & b'::tsquery = 'a & b'::tsquery)"));
    // PG preserves operand order — reordering makes them unequal.
    assert!(!b(&mut e, "SELECT ('a & b'::tsquery = 'b & a'::tsquery)"));
    assert!(b(&mut e, "SELECT ('a & b'::tsquery <> 'a & c'::tsquery)"));
    // Equal queries built the same way compare equal.
    assert!(b(
        &mut e,
        "SELECT (plainto_tsquery('simple','a b') = plainto_tsquery('simple','a b'))"
    ));
}
