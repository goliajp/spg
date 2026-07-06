//! v7.38 (read01 P4.11) — ROW(...) IS [NOT] NULL is field-wise: a row IS
//! NULL iff every field is null, IS NOT NULL iff every field is non-null
//! (so the two are not simple negations), and the test does not recurse
//! into nested rows. Verified vs live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::Bool(v) => v,
            ref v => panic!("expected bool, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn row_is_null_is_field_wise() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT ROW(NULL, NULL) IS NULL"));
    assert!(!b(&mut e, "SELECT ROW(1, NULL) IS NULL"));
    assert!(!b(&mut e, "SELECT ROW(1, 2) IS NULL"));
    // IS NOT NULL is "every field non-null", not the negation of IS NULL.
    assert!(!b(&mut e, "SELECT ROW(1, NULL) IS NOT NULL"));
    assert!(b(&mut e, "SELECT ROW(1, 2) IS NOT NULL"));
    assert!(!b(&mut e, "SELECT ROW(NULL, NULL) IS NOT NULL"));
    // Non-recursive: a nested row is a non-null field value.
    assert!(!b(&mut e, "SELECT (1, ROW(NULL, NULL)) IS NULL"));
    assert!(!b(&mut e, "SELECT ROW(NULL, ROW(NULL, NULL)) IS NULL"));
    // Arrays keep whole-value null semantics (unaffected).
    assert!(!b(&mut e, "SELECT ARRAY[NULL, NULL] IS NULL"));
}
