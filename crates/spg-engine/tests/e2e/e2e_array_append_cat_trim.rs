//! v7.37.17 (17.6 siblings) — array_append / array_prepend /
//! array_cat / trim_array (PG 14+).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_int_array(v: &spg_storage::Value<'_>) -> Vec<Option<i32>> {
    match v {
        spg_storage::Value::IntArray(items) => items.clone(),
        other => panic!("expected IntArray, got {other:?}"),
    }
}

fn as_text_array(v: &spg_storage::Value<'_>) -> Vec<Option<String>> {
    match v {
        spg_storage::Value::TextArray(items) => items.clone(),
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn array_append_int_and_text() {
    let mut e = Engine::new();
    // PG doc vector: array_append(ARRAY[1,2], 3) → {1,2,3}
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_append(ARRAY[1, 2], 3)")),
        [Some(1), Some(2), Some(3)]
    );
    // NULL element appends a NULL item.
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_append(ARRAY[1, 2], NULL)")),
        [Some(1), Some(2), None]
    );
    assert_eq!(
        as_text_array(&first(
            &mut e,
            "SELECT array_append(ARRAY['a', 'b'], 'c')"
        )),
        [
            Some("a".to_string()),
            Some("b".to_string()),
            Some("c".to_string())
        ]
    );
}

#[test]
fn array_prepend_int() {
    let mut e = Engine::new();
    // PG doc vector: array_prepend(1, ARRAY[2,3]) → {1,2,3}
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_prepend(1, ARRAY[2, 3])")),
        [Some(1), Some(2), Some(3)]
    );
}

#[test]
fn array_append_null_array_acts_as_empty() {
    let mut e = Engine::new();
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_append(NULL, 3)")),
        [Some(3)]
    );
    assert_eq!(
        as_text_array(&first(&mut e, "SELECT array_prepend('x', NULL)")),
        [Some("x".to_string())]
    );
}

#[test]
fn array_cat_int_and_text() {
    let mut e = Engine::new();
    // PG doc vector: array_cat(ARRAY[1,2,3], ARRAY[4,5]) → {1,2,3,4,5}
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT array_cat(ARRAY[1, 2, 3], ARRAY[4, 5])"
        )),
        [Some(1), Some(2), Some(3), Some(4), Some(5)]
    );
    assert_eq!(
        as_text_array(&first(
            &mut e,
            "SELECT array_cat(ARRAY['a'], ARRAY['b', 'c'])"
        )),
        [
            Some("a".to_string()),
            Some("b".to_string()),
            Some("c".to_string())
        ]
    );
    // NULL side yields the other side unchanged.
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_cat(NULL, ARRAY[1, 2])")),
        [Some(1), Some(2)]
    );
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT array_cat(ARRAY[1, 2], NULL)")),
        [Some(1), Some(2)]
    );
}

#[test]
fn trim_array_basic_and_errors() {
    let mut e = Engine::new();
    // PG doc vector: trim_array(ARRAY[1,2,3,4,5,6], 2) → {1,2,3,4}
    assert_eq!(
        as_int_array(&first(
            &mut e,
            "SELECT trim_array(ARRAY[1, 2, 3, 4, 5, 6], 2)"
        )),
        [Some(1), Some(2), Some(3), Some(4)]
    );
    // Trim to empty.
    assert_eq!(
        as_int_array(&first(&mut e, "SELECT trim_array(ARRAY[1, 2], 2)")),
        Vec::<Option<i32>>::new()
    );
    // Trim nothing.
    assert_eq!(
        as_text_array(&first(&mut e, "SELECT trim_array(ARRAY['a'], 0)")),
        [Some("a".to_string())]
    );
    // n out of range errors like PG.
    let err = e
        .execute("SELECT trim_array(ARRAY[1, 2], 3)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("between 0 and 2"),
        "unexpected error: {msg}"
    );
    let err = e
        .execute("SELECT trim_array(ARRAY[1, 2], -1)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("between 0 and 2"),
        "unexpected error: {msg}"
    );
}

#[test]
fn null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT trim_array(NULL, 1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT array_append(NULL, NULL)"),
        spg_storage::Value::Null
    ));
}
