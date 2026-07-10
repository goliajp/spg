//! Array operators && (overlap), @> / <@ (containment) — previously
//! misrouted to the inet / JSON interpretations.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match rows[0].values[0] {
        spg_storage::Value::Bool(v) => v,
        ref other => panic!("{sql}: expected Bool, got {other:?}"),
    }
}

#[test]
fn overlap() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT ARRAY[1,2] && ARRAY[2,3]"));
    assert!(!b(&mut e, "SELECT ARRAY[1,2] && ARRAY[3,4]"));
    assert!(b(&mut e, "SELECT ARRAY['a','b'] && ARRAY['b']"));
    // NULL elements never match.
    assert!(!b(
        &mut e,
        "SELECT ARRAY[NULL]::int[] && ARRAY[NULL]::int[]"
    ));
}

#[test]
fn containment() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT ARRAY[1,2,3] @> ARRAY[2]"));
    assert!(b(&mut e, "SELECT ARRAY[1,2,3] @> ARRAY[3,1]"));
    assert!(!b(&mut e, "SELECT ARRAY[1,2] @> ARRAY[4]"));
    assert!(b(&mut e, "SELECT ARRAY[2] <@ ARRAY[1,2,3]"));
    assert!(!b(&mut e, "SELECT ARRAY[4] <@ ARRAY[1,2,3]"));
    // Empty array is contained in everything.
    assert!(b(&mut e, "SELECT ARRAY[1,2] @> ARRAY[]::int[]"));
    // A NULL element can never be contained.
    assert!(!b(&mut e, "SELECT ARRAY[1,2] @> ARRAY[NULL]::int[]"));
    // JSON containment keeps working on JSON operands.
    assert!(b(&mut e, "SELECT '{\"a\":1}'::jsonb @> '{\"a\":1}'::jsonb"));
}
