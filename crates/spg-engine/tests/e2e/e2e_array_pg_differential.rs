//! v7.37.16 — array function/operator PG18 differential corpus.
//!
//! Each assertion below is the live PostgreSQL 18 answer (captured
//! on the mini bench container, `psql -tA`). This file pins the
//! corrections made in the same commit:
//!   * `array_length` on an empty array → NULL (not 0).
//!   * `array_position(arr, NULL)` matches a NULL element
//!     (IS NOT DISTINCT FROM semantics).
//!   * BOOL[] external text renders `t` / `f` (not `true`/`false`).
//!   * element-wise array ordering (`=` `<>` `<` `<=` `>` `>=`),
//!     with PG's NULL-as-greatest / shorter-is-less rules.
//! It also guards a set of behaviours that already matched PG so
//! they don't silently drift.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Render the single scalar cell of a one-row/one-col query the way
/// PG's `-tA` would: NULL → the literal `NULL`, booleans → `t`/`f`.
fn text1(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    assert_eq!(rows.len(), 1, "{sql}: expected exactly one row");
    match &rows[0].values[0] {
        Value::Null => "NULL".to_string(),
        Value::Text(s) => s.to_string(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

// ---- B1: array_length on empty array → NULL -----------------------

#[test]
fn array_length_empty_is_null() {
    let mut e = Engine::new();
    // PG18: SELECT array_length('{}'::int[], 1) → NULL
    assert_eq!(
        text1(&mut e, "SELECT array_length(ARRAY[]::int[], 1)"),
        "NULL"
    );
    assert_eq!(text1(&mut e, "SELECT array_length('{}'::int[], 1)"), "NULL");
    // Non-empty still works.
    assert_eq!(text1(&mut e, "SELECT array_length(ARRAY[1,2,3], 1)"), "3");
    // array_upper/lower already matched PG (NULL on empty).
    assert_eq!(
        text1(&mut e, "SELECT array_upper(ARRAY[]::int[], 1)"),
        "NULL"
    );
}

// ---- B2: array_position(arr, NULL) matches NULL element -----------

#[test]
fn array_position_null_needle_matches_null_element() {
    let mut e = Engine::new();
    // PG18: array_position(ARRAY[1,NULL,2], NULL) → 2
    assert_eq!(
        text1(&mut e, "SELECT array_position(ARRAY[1,NULL,2], NULL)"),
        "2"
    );
    // PG18: no NULL element → NULL.
    assert_eq!(
        text1(&mut e, "SELECT array_position(ARRAY[1,2,3], NULL::int)"),
        "NULL"
    );
    // Text array with a NULL element.
    assert_eq!(
        text1(&mut e, "SELECT array_position(ARRAY['a',NULL,'b'], NULL)"),
        "2"
    );
    // Non-NULL needle still skips NULL elements (unchanged behaviour).
    assert_eq!(
        text1(&mut e, "SELECT array_position(ARRAY[1,NULL,2], 2)"),
        "3"
    );
}

// ---- B3: BOOL[] external text renders t / f -----------------------

#[test]
fn bool_array_text_uses_t_f() {
    let mut e = Engine::new();
    // PG18: SELECT (ARRAY[true,false,NULL])::text → {t,f,NULL}
    assert_eq!(
        text1(&mut e, "SELECT (ARRAY[true,false,NULL])::text"),
        "{t,f,NULL}"
    );
    assert_eq!(text1(&mut e, "SELECT (ARRAY[false,true])::text"), "{f,t}");
}

// ---- B6: element-wise array comparison ----------------------------

#[test]
fn array_equality() {
    let mut e = Engine::new();
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2,3] = ARRAY[1,2,3]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2] = ARRAY[1,2,3]"), "f");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2,3] <> ARRAY[1,2,4]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2] <> ARRAY[1,2]"), "f");
    assert_eq!(text1(&mut e, "SELECT ARRAY['a','b'] = ARRAY['a','b']"), "t");
}

#[test]
fn array_ordering() {
    let mut e = Engine::new();
    // First differing element decides.
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2,3] < ARRAY[1,2,4]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,3] < ARRAY[1,2,3]"), "f");
    // Prefix: shorter is less.
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2] < ARRAY[1,2,3]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2,3] >= ARRAY[1,2,3]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[2,1] > ARRAY[1,9]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY['a','b'] < ARRAY['a','c']"), "t");
}

#[test]
fn array_comparison_null_semantics() {
    let mut e = Engine::new();
    // PG array_cmp: NULL element == NULL element (total order),
    // and NULL sorts as the greatest value.
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,NULL] = ARRAY[1,NULL]"), "t");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,NULL] = ARRAY[1,2]"), "f");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,NULL] < ARRAY[1,2]"), "f");
    assert_eq!(text1(&mut e, "SELECT ARRAY[1,2] < ARRAY[1,NULL]"), "t");
    assert_eq!(
        text1(&mut e, "SELECT ARRAY[NULL]::int[] = ARRAY[NULL]::int[]"),
        "t"
    );
}
