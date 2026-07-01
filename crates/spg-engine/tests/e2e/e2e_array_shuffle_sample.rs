//! v7.37.17 (17.6 siblings) — PG 16+ array_shuffle + array_sample.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn to_int_vec(v: &spg_storage::Value<'_>) -> Vec<i32> {
    match v {
        spg_storage::Value::IntArray(items) => items.iter().map(|o| o.unwrap()).collect(),
        other => panic!("expected IntArray, got {other:?}"),
    }
}

#[test]
fn array_shuffle_preserves_length_and_multiset() {
    let mut e = Engine::new();
    let src = to_int_vec(&first(&mut e, "SELECT ARRAY[1, 2, 3, 4, 5]::int[]"));
    let shuffled = to_int_vec(&first(&mut e, "SELECT array_shuffle(ARRAY[1, 2, 3, 4, 5])"));
    assert_eq!(shuffled.len(), src.len());
    let mut sorted_src = src;
    sorted_src.sort();
    let mut sorted_out = shuffled.clone();
    sorted_out.sort();
    assert_eq!(sorted_src, sorted_out);
}

#[test]
fn array_shuffle_empty_returns_empty() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT array_shuffle(ARRAY[]::int[])") {
        spg_storage::Value::IntArray(items) => assert!(items.is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_sample_returns_n_items() {
    let mut e = Engine::new();
    // Sample 3 from [1..10].
    match first(
        &mut e,
        "SELECT array_sample(ARRAY[1,2,3,4,5,6,7,8,9,10], 3)",
    ) {
        spg_storage::Value::IntArray(items) => {
            assert_eq!(items.len(), 3);
            // All samples should be from the source set.
            let source: std::collections::HashSet<i32> = (1..=10).collect();
            for o in items {
                let v = o.unwrap();
                assert!(source.contains(&v), "{v} not in source");
            }
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_sample_clamps_to_source_size() {
    let mut e = Engine::new();
    // Ask for 100 from a 5-element array — should return 5.
    match first(&mut e, "SELECT array_sample(ARRAY[1, 2, 3, 4, 5], 100)") {
        spg_storage::Value::IntArray(items) => assert_eq!(items.len(), 5),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn array_sample_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_sample(NULL::int[], 3)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT array_sample(ARRAY[1,2], NULL::int)"),
        spg_storage::Value::Null
    ));
}
