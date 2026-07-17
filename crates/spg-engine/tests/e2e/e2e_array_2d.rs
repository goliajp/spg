//! v7.38 (read01, T10) — multidimensional (2-D) arrays: the `ARRAY[[..],[..]]`
//! constructor builds a real 2-D array, and array_dims / array_ndims /
//! cardinality / array_length / array_upper / array_lower are dimension-aware.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Int(n) => n.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn array_2d_element_subscript() {
    // read01 2D-subscript — PG treats `arr[i][j]` as ONE 2-subscript op:
    // it reaches an element; a single subscript on a 2-D array is NULL (not
    // the row), and any out-of-range index is NULL. All values live-PG18.4.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])[1][2]"), "2");
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])[2][1]"), "3");
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[['a','b'],['c','d']])[2][2]"),
        "d"
    );
    // bigint matrix (values > i32::MAX force BigIntArray2D); cast to text
    // since the test helper has no BigInt arm.
    assert_eq!(
        text(&mut e, "SELECT ((ARRAY[[9999999999,2],[3,4]])[1][1])::text"),
        "9999999999"
    );
    // Partial subscript on a 2-D array → NULL (PG), not the sub-row.
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])[1]"), "Null");
    // Out-of-range → NULL.
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])[3][1]"), "Null");
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])[1][9]"), "Null");
    // Regression: 1-D subscript and chained JSON subscript still work.
    assert_eq!(text(&mut e, "SELECT (ARRAY[7,8,9])[2]"), "8");
    assert_eq!(text(&mut e, "SELECT (ARRAY[7,8,9])[9]"), "Null");
}

#[test]
fn array_2d_construct_and_introspect() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])::text"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[[1,2,3],[4,5,6]])::text"),
        "{{1,2,3},{4,5,6}}"
    );
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[['a','b'],['c','d']])::text"),
        "{{a,b},{c,d}}"
    );
    assert_eq!(
        text(&mut e, "SELECT array_dims(ARRAY[[1,2],[3,4]])"),
        "[1:2][1:2]"
    );
    assert_eq!(text(&mut e, "SELECT array_ndims(ARRAY[[1,2],[3,4]])"), "2");
    assert_eq!(text(&mut e, "SELECT cardinality(ARRAY[[1,2],[3,4]])"), "4");
    assert_eq!(
        text(&mut e, "SELECT array_length(ARRAY[[1,2,3],[4,5,6]],1)"),
        "2"
    );
    assert_eq!(
        text(&mut e, "SELECT array_length(ARRAY[[1,2,3],[4,5,6]],2)"),
        "3"
    );
    assert_eq!(
        text(&mut e, "SELECT array_upper(ARRAY[[1,2,3],[4,5,6]],2)"),
        "3"
    );
    assert_eq!(
        text(&mut e, "SELECT array_lower(ARRAY[[1,2,3],[4,5,6]],1)"),
        "1"
    );
    // Ragged sub-arrays error.
    assert!(e.execute("SELECT ARRAY[[1,2],[3]]").is_err());
    // 1-D arrays and pgvector literals are unaffected.
    assert_eq!(text(&mut e, "SELECT array_ndims(ARRAY[1,2,3])"), "1");
    assert_eq!(text(&mut e, "SELECT array_dims(ARRAY[1,2,3])"), "[1:3]");
    // pgvector literal `[...]` is unaffected by the ARRAY[[..]] nesting
    // change; the text form is pgvector's spaceless vector_out.
    assert_eq!(text(&mut e, "SELECT ('[1,2,3]'::vector)::text"), "[1,2,3]");
}
