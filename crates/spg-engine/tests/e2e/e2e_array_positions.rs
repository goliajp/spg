//! v7.37.17 (17.6 siblings) — array_positions.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn array_positions_int_multiple_matches() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_positions(ARRAY[1, 2, 3, 2, 4, 2], 2)");
    match &v {
        spg_storage::Value::IntArray(items) => {
            let s: Vec<_> = items.iter().map(|o| o.unwrap()).collect();
            assert_eq!(s, [2, 4, 6]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_positions_text_single_match() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_positions(ARRAY['a', 'b', 'a'], 'b')");
    match &v {
        spg_storage::Value::IntArray(items) => {
            let s: Vec<_> = items.iter().map(|o| o.unwrap()).collect();
            assert_eq!(s, [2]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_positions_no_matches_empty_array() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT array_positions(ARRAY[1, 2, 3], 99)");
    match &v {
        spg_storage::Value::IntArray(items) => assert!(items.is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_positions_null_array_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_positions(NULL::int[], 1)"),
        spg_storage::Value::Null
    ));
}
