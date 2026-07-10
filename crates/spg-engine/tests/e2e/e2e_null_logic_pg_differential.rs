//! v7.37.16 — boolean / NULL three-valued-logic (3VL) PG18 differential corpus.
//!
//! Every assertion below is the live PostgreSQL 18 answer (captured on the
//! mini bench container, `psql -tA` with `coalesce((expr)::text,'NULL')` to
//! make NULL visible). This is the 9th SPG differential sweep; the 3VL surface
//! (AND/OR/NOT with NULL, comparison-to-NULL, IS [NOT] DISTINCT, IS
//! TRUE/FALSE/UNKNOWN, IN/NOT IN with NULL, ANY/ALL with NULL, CASE null
//! conditions, COALESCE/NULLIF/GREATEST/LEAST, boolean casts, WHERE-clause
//! filtering, bool_and/bool_or) matched PG18 on **all 101** probed cases —
//! no correctness divergence was found.
//!
//! Wrong NULL handling silently corrupts WHERE / CASE results, so this file
//! pins the matched behaviour against future drift.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Render the single scalar cell the way PG's `-tA` would for a boolean-typed
/// column: NULL → `NULL`, booleans → `t`/`f`, everything else as text.
fn b1(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    assert_eq!(rows.len(), 1, "{sql}: expected exactly one row");
    match &rows[0].values[0] {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

// ---- AND / OR / NOT truth tables with NULL ------------------------

#[test]
fn boolean_connectives_with_null() {
    let mut e = Engine::new();
    // NULL AND true → NULL, but NULL AND false → false (short-circuit).
    assert_eq!(b1(&mut e, "SELECT (NULL AND true)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULL AND false)"), "f");
    assert_eq!(b1(&mut e, "SELECT (true AND NULL)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (false AND NULL)"), "f");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool AND NULL::bool)"), "NULL");
    // NULL OR true → true (short-circuit), NULL OR false → NULL.
    assert_eq!(b1(&mut e, "SELECT (NULL OR true)"), "t");
    assert_eq!(b1(&mut e, "SELECT (NULL OR false)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (true OR NULL)"), "t");
    assert_eq!(b1(&mut e, "SELECT (false OR NULL)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool OR NULL::bool)"), "NULL");
    // NOT NULL → NULL.
    assert_eq!(b1(&mut e, "SELECT (NOT NULL::bool)"), "NULL");
}

// ---- comparison against NULL yields NULL --------------------------

#[test]
fn comparison_with_null_is_null() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (1 = NULL)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULL = NULL)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (1 <> NULL)"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULL < 1)"), "NULL");
}

// ---- IS [NOT] NULL / IS [NOT] DISTINCT FROM -----------------------

#[test]
fn is_null_and_is_distinct_from() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (NULL IS NULL)"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 IS NOT NULL)"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 IS DISTINCT FROM NULL)"), "t");
    assert_eq!(b1(&mut e, "SELECT (NULL IS DISTINCT FROM NULL)"), "f");
    assert_eq!(b1(&mut e, "SELECT (NULL IS NOT DISTINCT FROM NULL)"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 IS NOT DISTINCT FROM 1)"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 IS DISTINCT FROM 2)"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 IS DISTINCT FROM 1)"), "f");
}

// ---- IS TRUE / FALSE / UNKNOWN ------------------------------------

#[test]
fn is_true_false_unknown() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS TRUE)"), "f");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS NOT TRUE)"), "t");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS FALSE)"), "f");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS NOT FALSE)"), "t");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS UNKNOWN)"), "t");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool IS NOT UNKNOWN)"), "f");
    assert_eq!(b1(&mut e, "SELECT ((1=1) IS TRUE)"), "t");
    assert_eq!(b1(&mut e, "SELECT ((1=2) IS FALSE)"), "t");
    assert_eq!(b1(&mut e, "SELECT (true IS TRUE)"), "t");
    assert_eq!(b1(&mut e, "SELECT (false IS FALSE)"), "t");
}

// ---- IN / NOT IN with NULL (the classic gotchas) ------------------

#[test]
fn in_not_in_with_null() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (1 IN (1,2))"), "t");
    // no match + a NULL in the list → NULL, not false.
    assert_eq!(b1(&mut e, "SELECT (3 IN (1,2,NULL))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (1 IN (2,NULL))"), "NULL");
    // a real match short-circuits the NULL → true.
    assert_eq!(b1(&mut e, "SELECT (1 IN (1,NULL))"), "t");
    // NOT IN with a NULL and no equal → NULL, not true (the trap).
    assert_eq!(b1(&mut e, "SELECT (1 NOT IN (2,NULL))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (1 NOT IN (2,3))"), "t");
    // NOT IN with a real match → false regardless of the NULL.
    assert_eq!(b1(&mut e, "SELECT (1 NOT IN (1,NULL))"), "f");
    assert_eq!(b1(&mut e, "SELECT (NULL IN (1,2))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULL NOT IN (1,2))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (3 NOT IN (1,2,NULL))"), "NULL");
}

// ---- ANY / ALL with NULL ------------------------------------------

#[test]
fn any_all_with_null() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (1 = ANY(ARRAY[1,NULL]))"), "t");
    assert_eq!(b1(&mut e, "SELECT (3 = ANY(ARRAY[1,NULL]))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (3 = ANY(ARRAY[1,2]))"), "f");
    assert_eq!(b1(&mut e, "SELECT (1 <> ALL(ARRAY[2,NULL]))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (5 > ALL(ARRAY[1,2]))"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 > ALL(ARRAY[1,2]))"), "f");
    assert_eq!(b1(&mut e, "SELECT (1 = ALL(ARRAY[1,1]))"), "t");
    assert_eq!(b1(&mut e, "SELECT (1 = ALL(ARRAY[1,NULL]))"), "NULL");
    // empty array: ANY → false, ALL → true.
    assert_eq!(b1(&mut e, "SELECT (3 = ANY(ARRAY[]::int[]))"), "f");
    assert_eq!(b1(&mut e, "SELECT (3 <> ALL(ARRAY[]::int[]))"), "t");
}

// ---- CASE with NULL conditions ------------------------------------

#[test]
fn case_with_null_conditions() {
    let mut e = Engine::new();
    // NULL condition is "not true" → ELSE branch.
    assert_eq!(
        b1(&mut e, "SELECT (CASE WHEN NULL THEN 'a' ELSE 'b' END)"),
        "b"
    );
    assert_eq!(
        b1(&mut e, "SELECT (CASE WHEN 1=NULL THEN 'a' ELSE 'b' END)"),
        "b"
    );
    // searched CASE: NULL never equals NULL → ELSE.
    assert_eq!(
        b1(&mut e, "SELECT (CASE 1 WHEN NULL THEN 'a' ELSE 'b' END)"),
        "b"
    );
    assert_eq!(
        b1(&mut e, "SELECT (CASE NULL WHEN NULL THEN 'a' ELSE 'b' END)"),
        "b"
    );
    assert_eq!(
        b1(&mut e, "SELECT (CASE WHEN true THEN NULL END)::text"),
        "NULL"
    );
    assert_eq!(b1(&mut e, "SELECT (CASE WHEN false THEN 'a' END)"), "NULL");
}

// ---- COALESCE / NULLIF / GREATEST / LEAST -------------------------

#[test]
fn coalesce_nullif_greatest_least() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT (COALESCE(NULL,NULL,3))"), "3");
    assert_eq!(b1(&mut e, "SELECT (COALESCE(NULL::int,NULL))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (COALESCE(NULL,2,3))"), "2");
    assert_eq!(b1(&mut e, "SELECT (NULLIF(1,1))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (NULLIF(1,2))"), "1");
    assert_eq!(b1(&mut e, "SELECT (NULLIF(NULL::int,1))"), "NULL");
    // GREATEST/LEAST skip NULLs in PG (NULL only if all args NULL).
    assert_eq!(b1(&mut e, "SELECT (GREATEST(1,NULL,3))"), "3");
    assert_eq!(b1(&mut e, "SELECT (GREATEST(NULL,2))"), "2");
    assert_eq!(b1(&mut e, "SELECT (GREATEST(NULL::int,NULL))"), "NULL");
    assert_eq!(b1(&mut e, "SELECT (LEAST(1,NULL,3))"), "1");
    assert_eq!(b1(&mut e, "SELECT (LEAST(NULL::int,NULL))"), "NULL");
}

// ---- boolean casts / literals -------------------------------------

#[test]
fn boolean_casts_and_literals() {
    let mut e = Engine::new();
    assert_eq!(b1(&mut e, "SELECT ('t'::bool)"), "t");
    assert_eq!(b1(&mut e, "SELECT ('yes'::bool)"), "t");
    assert_eq!(b1(&mut e, "SELECT ('1'::bool)"), "t");
    assert_eq!(b1(&mut e, "SELECT ('no'::bool)"), "f");
    assert_eq!(b1(&mut e, "SELECT ('off'::bool)"), "f");
    assert_eq!(b1(&mut e, "SELECT (true::int)"), "1");
    assert_eq!(b1(&mut e, "SELECT (false::int)"), "0");
    assert_eq!(b1(&mut e, "SELECT (1::bool)"), "t");
    assert_eq!(b1(&mut e, "SELECT (0::bool)"), "f");
    // bool → text renders `true`/`false` (not `t`/`f`).
    assert_eq!(b1(&mut e, "SELECT (true::text)"), "true");
    assert_eq!(b1(&mut e, "SELECT (false::text)"), "false");
    assert_eq!(b1(&mut e, "SELECT (NULL::bool)"), "NULL");
}

// ---- WHERE-clause filtering + aggregates over NULL bool -----------
//
// The high-impact surface: a NULL predicate must exclude the row, and
// NOT IN against a NULL-bearing subquery must return no rows.

#[test]
fn where_filtering_and_bool_aggregates() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t3vl (id int, v int, b bool)")
        .unwrap();
    e.execute(
        "INSERT INTO t3vl VALUES (1,10,true),(2,NULL,false),(3,30,NULL),(4,NULL,NULL),(5,50,true)",
    )
    .unwrap();

    let agg = |e: &mut Engine, sql: &str| -> String {
        b1(
            e,
            &format!(
                "SELECT coalesce(string_agg(id::text,',' ORDER BY id),'<empty>') FROM t3vl {sql}"
            ),
        )
    };

    assert_eq!(agg(&mut e, "WHERE v > 20"), "3,5");
    assert_eq!(agg(&mut e, "WHERE v = NULL"), "<empty>");
    assert_eq!(agg(&mut e, "WHERE NOT (v > 20)"), "1");
    assert_eq!(agg(&mut e, "WHERE b"), "1,5");
    assert_eq!(agg(&mut e, "WHERE NOT b"), "2");
    assert_eq!(agg(&mut e, "WHERE b IS NOT TRUE"), "2,3,4");
    assert_eq!(agg(&mut e, "WHERE v IN (10,30)"), "1,3");
    assert_eq!(agg(&mut e, "WHERE v NOT IN (10,30)"), "5");
    // NOT IN against a subquery that yields a NULL → whole predicate NULL → no rows.
    assert_eq!(
        agg(
            &mut e,
            "WHERE v NOT IN (SELECT v FROM t3vl WHERE id IN (1,2))"
        ),
        "<empty>",
    );
    assert_eq!(agg(&mut e, "WHERE v IS DISTINCT FROM 10"), "2,3,4,5");
    assert_eq!(
        agg(&mut e, "WHERE (CASE WHEN v > 20 THEN true ELSE false END)"),
        "3,5",
    );

    // bool_and / bool_or ignore NULLs; all-NULL group → NULL.
    assert_eq!(b1(&mut e, "SELECT bool_and(b) FROM t3vl"), "f");
    assert_eq!(b1(&mut e, "SELECT bool_or(b) FROM t3vl"), "t");
    assert_eq!(
        b1(&mut e, "SELECT bool_and(b) FROM t3vl WHERE b IS NOT NULL"),
        "f"
    );
    assert_eq!(
        b1(
            &mut e,
            "SELECT coalesce(bool_and(b)::text,'NULL') FROM t3vl WHERE id=4"
        ),
        "NULL",
    );

    // count(*) counts rows; count(col) skips NULLs.
    assert_eq!(b1(&mut e, "SELECT count(*) FROM t3vl"), "5");
    assert_eq!(b1(&mut e, "SELECT count(v) FROM t3vl"), "3");
    assert_eq!(b1(&mut e, "SELECT count(b) FROM t3vl"), "3");
}
