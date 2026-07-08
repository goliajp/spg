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
fn array_2d_construct_and_introspect() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2],[3,4]])::text"), "{{1,2},{3,4}}");
    assert_eq!(text(&mut e, "SELECT (ARRAY[[1,2,3],[4,5,6]])::text"), "{{1,2,3},{4,5,6}}");
    assert_eq!(text(&mut e, "SELECT (ARRAY[['a','b'],['c','d']])::text"), "{{a,b},{c,d}}");
    assert_eq!(text(&mut e, "SELECT array_dims(ARRAY[[1,2],[3,4]])"), "[1:2][1:2]");
    assert_eq!(text(&mut e, "SELECT array_ndims(ARRAY[[1,2],[3,4]])"), "2");
    assert_eq!(text(&mut e, "SELECT cardinality(ARRAY[[1,2],[3,4]])"), "4");
    assert_eq!(text(&mut e, "SELECT array_length(ARRAY[[1,2,3],[4,5,6]],1)"), "2");
    assert_eq!(text(&mut e, "SELECT array_length(ARRAY[[1,2,3],[4,5,6]],2)"), "3");
    assert_eq!(text(&mut e, "SELECT array_upper(ARRAY[[1,2,3],[4,5,6]],2)"), "3");
    assert_eq!(text(&mut e, "SELECT array_lower(ARRAY[[1,2,3],[4,5,6]],1)"), "1");
    // Ragged sub-arrays error.
    assert!(e.execute("SELECT ARRAY[[1,2],[3]]").is_err());
    // 1-D arrays and pgvector literals are unaffected.
    assert_eq!(text(&mut e, "SELECT array_ndims(ARRAY[1,2,3])"), "1");
    assert_eq!(text(&mut e, "SELECT array_dims(ARRAY[1,2,3])"), "[1:3]");
    // pgvector literal `[...]` is unaffected by the ARRAY[[..]] nesting change.
    assert_eq!(text(&mut e, "SELECT ('[1,2,3]'::vector)::text"), "[1, 2, 3]");
}
