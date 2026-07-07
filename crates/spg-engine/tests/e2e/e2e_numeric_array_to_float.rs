//! v7.38 (read01 sweep) — a NUMERIC[] value coerces element-wise into a
//! float8[] target: both an explicit `::float8[]` cast and an INSERT into a
//! float8[] column (PG accepts `ARRAY[1.5::numeric]::float8[]` and the
//! equivalent insert). Oracle: live PG 18.4.

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
fn numeric_array_coerces_into_float8_array() {
    let mut e = Engine::new();
    // Explicit cast.
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[2.25::numeric, 3.5::numeric]::float8[])::text"),
        "{2.25,3.5}"
    );
    // INSERT into a float8[] column coerces the numeric array element-wise.
    e.execute("CREATE TABLE fa (xs float8[])").unwrap();
    e.execute("INSERT INTO fa VALUES (ARRAY[2.25::numeric, 3.5::numeric])")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT xs::text FROM fa"), "{2.25,3.5}");
    // Whole-number numeric elements widen cleanly too.
    e.execute("CREATE TABLE fb (xs float8[])").unwrap();
    e.execute("INSERT INTO fb VALUES (ARRAY[1::numeric, 2::numeric])")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT xs::text FROM fb"), "{1,2}");
}
