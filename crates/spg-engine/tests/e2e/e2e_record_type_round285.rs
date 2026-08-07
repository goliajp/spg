//! v7.39 (round 285) — `::record`, and the three ways PG words a missing
//! composite field.
//!
//! `record` is PG's anonymous composite type. It is not a catalog object,
//! so the Named-cast lookups (domain / enum / composite) all miss it and
//! `ROW(1,2)::record` answered "unsupported cast target". PG treats the
//! cast as an IDENTITY on anything already composite — the value keeps its
//! fields AND their names, which is why `(ROW(1,2)::record).f1` still
//! resolves.
//!
//! The missing-field message turned out to have three shapes, chosen by
//! the base expression's STATIC type rather than by the value. A
//! `Value::Composite` carries field names but not a type name, so the
//! expression is what decides.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows[0]
        .values
        .iter()
        .map(spg_engine::eval::value_to_text)
        .collect::<Vec<_>>()
        .join("|")
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}")
            .replace("eval: type mismatch: ", "")
            .replace("unsupported: ", ""),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE rc9 AS (a text, b int)").unwrap();
    e.execute("CREATE TABLE rt8 (x int, y text)").unwrap();
    e.execute("INSERT INTO rt8 VALUES (1,'p')").unwrap();
    e
}

#[test]
fn record_is_an_identity_cast_on_a_composite() {
    let mut e = fixture();
    assert_eq!(one(&mut e, "SELECT ROW(1,2)::record"), "(1,2)");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ROW(1,2)::record)"), "record");
    assert_eq!(one(&mut e, "SELECT (SELECT ROW('a',1)::record)"), "(a,1)");
    assert_eq!(one(&mut e, "SELECT ROW(1,2)::record::text"), "(1,2)");
}

#[test]
fn the_fields_survive_the_cast() {
    // The point of "identity": field names are kept, so extraction and
    // equality still work through the cast.
    let mut e = fixture();
    assert_eq!(one(&mut e, "SELECT (ROW(1,2)::record).f1"), "1");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof((ROW('a'::text,1)::record).f1)"),
        "text",
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT ROW('a'::text,1)::record = ROW('a'::text,1)::record",
        ),
        "true",
    );
}

#[test]
fn a_non_composite_is_refused_with_pgs_wording() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT 1::record"),
        "cannot cast type integer to record",
    );
}

#[test]
fn a_named_composite_names_itself_in_the_missing_field_error() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT (ROW('a'::text,1)::rc9).nosuch"),
        "column \"nosuch\" not found in data type rc9",
    );
}

#[test]
fn a_whole_row_reference_uses_the_qualified_unquoted_wording() {
    // The odd one out: qualified with the relation, and NOT quoted.
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT (rt8).nosuch FROM rt8"),
        "column rt8.nosuch does not exist",
    );
}

#[test]
fn an_anonymous_row_says_it_could_not_identify_the_column() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT (ROW('a',1)).nosuch"),
        "could not identify column \"nosuch\" in record data type",
    );
    assert_eq!(
        err(&mut e, "SELECT (ROW('a'::text,1)::record).nosuch"),
        "could not identify column \"nosuch\" in record data type",
    );
}

#[test]
fn the_named_composite_forms_still_work() {
    // Guard the surface the cast arm sits next to — a `::record` branch
    // placed ahead of the catalog lookups would have swallowed these.
    let mut e = fixture();
    assert_eq!(one(&mut e, "SELECT (ROW('a'::text,1)::rc9).a"), "a");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(ROW('a'::text,1)::rc9)"),
        "rc9"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (r).a, (r).b FROM (SELECT ROW('x'::text,9)::rc9 AS r) s"
        ),
        "x|9",
    );
    assert_eq!(
        one(&mut e, "SELECT row_to_json(ROW('a'::text,1)::rc9)"),
        "{\"a\":\"a\",\"b\":1}",
    );
}
