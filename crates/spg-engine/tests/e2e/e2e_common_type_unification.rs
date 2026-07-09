//! v7.38 (read01) — PG common-type resolution for CASE / COALESCE /
//! GREATEST / LEAST / NULLIF. When branches mix integer and numeric (or
//! date and timestamp), the result takes PG's common type, so a branch
//! taken as integer is widened — `pg_typeof` matches PG and, crucially,
//! downstream division is numeric (`0.5`) not integer (`0`). Every expected
//! value / type is byte-for-byte from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                other => panic!("{sql}: expected Text, got {other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn case_result_is_common_type() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 2.5 END)::text"), "numeric");
    // The taken integer branch is widened → numeric division, not integer.
    assert_eq!(
        one(&mut e, "SELECT ((CASE WHEN true THEN 1 ELSE 2.5 END) / 2)::text"),
        "0.50000000000000000000"
    );
    // int ∪ bigint stays bigint; int ∪ float8 becomes double.
    assert_eq!(one(&mut e, "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 2::bigint END)::text"), "bigint");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 2.5::float8 END)::text"),
        "double precision"
    );
    // Uniform branches are untouched (no spurious widening).
    assert_eq!(one(&mut e, "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 2 END)::text"), "integer");
}

#[test]
fn coalesce_greatest_least_nullif_common_type() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(COALESCE(1, 2.5))::text"), "numeric");
    assert_eq!(one(&mut e, "SELECT (COALESCE(1, 2.5) / 2)::text"), "0.50000000000000000000");
    assert_eq!(one(&mut e, "SELECT (COALESCE(2.5, 1))::text"), "2.5");
    assert_eq!(one(&mut e, "SELECT pg_typeof(GREATEST(3, 2.5))::text"), "numeric");
    assert_eq!(one(&mut e, "SELECT (GREATEST(3, 2.5))::text"), "3");
    assert_eq!(one(&mut e, "SELECT (LEAST(3, 2.5))::text"), "2.5");
    assert_eq!(one(&mut e, "SELECT pg_typeof(NULLIF(1, 2.5))::text"), "numeric");
    assert_eq!(one(&mut e, "SELECT (NULLIF(1, 2.5))::text"), "1");
    // All-integer NULLIF stays integer.
    assert_eq!(one(&mut e, "SELECT pg_typeof(NULLIF(5, 2))::text"), "integer");
}

#[test]
fn least_greatest_temporal_common_type() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(LEAST('2020-01-01'::date, '2020-06-01'::timestamp))::text"),
        "timestamp without time zone"
    );
}

#[test]
fn compiled_path_widens_through_a_table() {
    // A FROM table drives the Step-VM (compiled) path, where the common type
    // is resolved once at compile time and coerced per row. Oracle: PG18.4.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct(a int, b numeric)").unwrap();
    e.execute("INSERT INTO ct VALUES (5, 2.5), (-1, 4.0)").unwrap();
    assert_eq!(
        col(&mut e, "SELECT ((CASE WHEN a > 0 THEN a ELSE b END) / 2)::text FROM ct ORDER BY a"),
        vec!["2.0000000000000000", "2.5000000000000000"]
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(CASE WHEN a > 0 THEN a ELSE b END)::text FROM ct LIMIT 1"),
        "numeric"
    );
    assert_eq!(
        col(&mut e, "SELECT (COALESCE(a, b) / 2)::text FROM ct ORDER BY a"),
        vec!["-0.50000000000000000000", "2.5000000000000000"]
    );
    assert_eq!(
        col(&mut e, "SELECT (GREATEST(a, b))::text FROM ct ORDER BY a"),
        vec!["4.0", "5"]
    );
}
