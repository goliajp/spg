//! v7.39 (round 264) — closes the composite residuals round 263 recorded:
//! NESTED composites and composite-returning scalar subqueries.
//!
//! A composite field whose type is ITSELF a composite resolved to the
//! parser's Text placeholder, because `CompositeDef.fields` is
//! `(name, DataType)` and a user type has no room in the DataType
//! lattice. The inner record therefore stayed TEXT: `(x).inner.street`
//! errored, `pg_typeof` said text, and `row_to_json` nested a STRING
//! instead of an object. This is the third instance of one shape this
//! campaign — an enum (round 258), a domain over a domain (round 259)
//! and now a composite field all needed the user type's NAME carried
//! alongside the DataType. `CompositeDef.field_user_types`, catalog
//! FILE_VERSION 76, with the cast and the rehydration both recursing.
//!
//! A composite-returning scalar subquery was refused outright
//! ("subquery result type None not yet materialisable"); it now
//! materialises back through a ROW constructor of its field literals.
//!
//! Recorded residuals, all probed, all needing STATIC type resolution
//! of an expression: `pg_typeof((…).inner_a)` reports `record`; a
//! subquery's materialised ROW loses the field NAMES so
//! `((SELECT ROW('a',1)::addr)).street` cannot resolve; and
//! field-not-found still says `column "nosuch" does not exist` rather
//! than PG's `column "nosuch" not found in data type addr`.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE paddr5 AS (street text, zip int)")
        .unwrap();
    e.execute("CREATE TYPE pnested AS (inner_a paddr5, n int)")
        .unwrap();
    e.execute("CREATE TABLE pnt (id int, v pnested)").unwrap();
    e.execute("INSERT INTO pnt VALUES (1, ROW(ROW('elm',9)::paddr5, 3))")
        .unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
            other => spg_engine::eval::value_to_text(other),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_nested_composite_stays_a_record_in_expressions() {
    let mut e = seeded();
    assert_eq!(
        one(&mut e, "SELECT ROW(ROW('s',1)::paddr5, 7)::pnested"),
        "(\"(s,1)\",7)"
    );
    // The inner field is a RECORD, so it can be walked into.
    assert_eq!(
        one(
            &mut e,
            "SELECT ((ROW(ROW('s',1)::paddr5, 7)::pnested).inner_a).street"
        ),
        "s"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (ROW(ROW('s',1)::paddr5, 7)::pnested).inner_a"
        ),
        "(s,1)"
    );
}

#[test]
fn a_nested_composite_column_round_trips() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT v FROM pnt"), "(\"(elm,9)\",3)");
    assert_eq!(one(&mut e, "SELECT ((v).inner_a).street FROM pnt"), "elm");
    // The inner field keeps its declared type, so this is arithmetic.
    assert_eq!(one(&mut e, "SELECT ((v).inner_a).zip + 1 FROM pnt"), "10");
    assert_eq!(one(&mut e, "SELECT (v).n FROM pnt"), "3");
    // row_to_json nests an OBJECT, not a string.
    assert_eq!(
        one(&mut e, "SELECT row_to_json(v) FROM pnt"),
        "{\"inner_a\":{\"street\":\"elm\",\"zip\":9},\"n\":3}"
    );
}

#[test]
fn a_composite_returning_subquery_materialises() {
    let mut e = seeded();
    assert_eq!(
        one(&mut e, "SELECT (SELECT ROW('a',1)::paddr5) IS NOT NULL"),
        "t"
    );
    assert_eq!(one(&mut e, "SELECT (SELECT ROW('a',1)::paddr5)"), "(a,1)");
}
