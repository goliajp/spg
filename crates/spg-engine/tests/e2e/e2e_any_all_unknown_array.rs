//! v7.38 (read01) — `x op ANY/ALL (rhs)` where rhs is an unknown-string array
//! literal (`3 = ANY('{1,2,3}')`). The text takes the LHS's type (PG's
//! implicit unknown → array coercion), and the element list is now built
//! generically so numeric[]/float8[]/bool[]/date[] arrays work too, not just
//! int/bigint/text. Every value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Null => "NULL".to_string(),
            other => panic!("{sql}: expected Text/Null, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn any_all_over_unknown_string_array() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (3 = ANY('{1,2,3}'))::text"), "true");
    assert_eq!(one(&mut e, "SELECT (5 = ANY('{1,2,3}'))::text"), "false");
    assert_eq!(one(&mut e, "SELECT ('x' = ANY('{x,y}'))::text"), "true");
    assert_eq!(one(&mut e, "SELECT (5 = ALL('{5,5}'))::text"), "true");
    assert_eq!(one(&mut e, "SELECT (3 > ANY('{1,2}'))::text"), "true");
    assert_eq!(one(&mut e, "SELECT (5 > ALL('{1,2,3}'))::text"), "true");
    // Non-int element types now work too.
    assert_eq!(one(&mut e, "SELECT (2.5 = ANY('{1.5,2.5}'))::text"), "true");
    assert_eq!(one(&mut e, "SELECT (true = ANY('{t,f}'))::text"), "true");
    assert_eq!(
        one(
            &mut e,
            "SELECT ('2020-01-01'::date = ANY('{2020-01-01,2021-06-15}'))::text"
        ),
        "true"
    );
    // A NULL LHS resolves to NULL; an empty array is false for ANY.
    assert_eq!(one(&mut e, "SELECT (NULL = ANY('{1,2}'))::text"), "NULL");
    assert_eq!(one(&mut e, "SELECT (1 = ANY('{}'))::text"), "false");
    // An explicit array cast and an ARRAY[] constructor still work.
    assert_eq!(
        one(&mut e, "SELECT (3 = ANY('{1,2,3}'::int[]))::text"),
        "true"
    );
    assert_eq!(one(&mut e, "SELECT (3 = ANY(ARRAY[1,2,3]))::text"), "true");
}
