//! v7.37.17 (17.6 siblings) — array_remove + array_replace (int/bigint).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_int_array(v: &spg_storage::Value<'_>) -> Vec<i32> {
    match v {
        spg_storage::Value::IntArray(items) => items.iter().map(|o| o.unwrap()).collect(),
        other => panic!("expected IntArray, got {other:?}"),
    }
}

#[test]
fn array_remove_int() {
    let mut e = Engine::new();
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT array_remove(ARRAY[1, 2, 3, 2, 4, 2], 2)"
        )),
        [1, 3, 4]
    );
    // Empty result.
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_remove(ARRAY[7, 7, 7], 7)")),
        Vec::<i32>::new()
    );
    // No match — unchanged.
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT array_remove(ARRAY[1, 2, 3], 99)"
        )),
        [1, 2, 3]
    );
}

#[test]
fn array_replace_int() {
    let mut e = Engine::new();
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT array_replace(ARRAY[1, 2, 3, 2], 2, 99)"
        )),
        [1, 99, 3, 99]
    );
    // No from match — unchanged.
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT array_replace(ARRAY[1, 2, 3], 99, 42)"
        )),
        [1, 2, 3]
    );
}

#[test]
fn array_remove_null_array_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_remove(NULL::int[], 1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT array_replace(NULL::int[], 1, 2)"),
        spg_storage::Value::Null
    ));
}
