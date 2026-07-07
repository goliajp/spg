//! v7.38 (read01 sweep) — the numeric-array coercion matrix PG accepts on
//! INSERT / cast: int[]→bigint[], int[]/bigint[]→numeric[], float8[]→numeric[],
//! and the narrowing bigint[]→int[] (which rejects an out-of-range element).
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn numeric_array_coercion_matrix() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ai (v bigint[])").unwrap();
    e.execute("INSERT INTO ai VALUES (ARRAY[1::int, 2::int])").unwrap();
    assert_eq!(text(&mut e, "SELECT v::text FROM ai"), "{1,2}");

    e.execute("CREATE TABLE an (v numeric[])").unwrap();
    e.execute("INSERT INTO an VALUES (ARRAY[3::int, 4::int])").unwrap();
    e.execute("INSERT INTO an VALUES (ARRAY[5::bigint, 6::bigint])").unwrap();
    e.execute("INSERT INTO an VALUES (ARRAY[1.5::float8, 2.5::float8])").unwrap();
    assert_eq!(text(&mut e, "SELECT v::text FROM an WHERE v[1] = 3"), "{3,4}");
    assert_eq!(text(&mut e, "SELECT v::text FROM an WHERE v[1] = 5"), "{5,6}");
    assert_eq!(text(&mut e, "SELECT v::text FROM an WHERE v[1] = 1.5"), "{1.5,2.5}");

    e.execute("CREATE TABLE aj (v int[])").unwrap();
    e.execute("INSERT INTO aj VALUES (ARRAY[7::bigint, 8::bigint])").unwrap();
    assert_eq!(text(&mut e, "SELECT v::text FROM aj"), "{7,8}");
    // An out-of-int-range bigint element is rejected, matching PG.
    assert!(e
        .execute("INSERT INTO aj VALUES (ARRAY[9999999999::bigint])")
        .is_err());
}
