//! v7.39 (round 648) — `regtype` was the one of the three that did not
//! carry its oid.
//!
//! `Value::RegClass` and `Value::RegProc` both hold `(oid, name)`, and
//! their doc comments say why: without the oid half a catalog join
//! cannot happen, and without the name half the value does not render.
//! `::regtype` produced a plain `Value::Text` holding the canonical
//! name, which rendered correctly and failed everything downstream —
//! `'text'::regtype::oid` parsed the NAME as a number and answered
//! `invalid input syntax for type oid: "text"` where PG answers 25, and
//! `pg_typeof` on one said `text`.
//!
//! This is F22, which the checklist had carried as open since round 621
//! with the diagnosis "标量与数组一样,故非数组缺口" — right that it was
//! not an array gap, and one layer short of the cause.
//!
//! Found by reconciling the checklist against the binary rather than by
//! reading it: of the 52 items it called open, six were already closed
//! and one (F18) appeared twice, once in each state.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn round648_regtype_carries_its_oid() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'text'::regtype::oid"), "25");
    assert_eq!(one(&mut e, "SELECT 'int4'::regtype::int"), "23");
    assert_eq!(one(&mut e, "SELECT 'text'::regtype::oid = 25"), "true");
    // …while still rendering as the name, which is the half it always had.
    assert_eq!(one(&mut e, "SELECT 'text'::regtype::text"), "text");
    // The canonicalisation survives: int4 renders as integer.
    assert_eq!(one(&mut e, "SELECT 'int4'::regtype::text"), "integer");
}

#[test]
fn round648_regtype_says_it_is_a_regtype() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('text'::regtype)"), "regtype");
    // Its two siblings were already right; the point is that all three
    // now agree on the shape.
    assert_eq!(one(&mut e, "SELECT 'pg_class'::regclass::oid"), "1259");
}

#[test]
fn round648_the_oid_direction_still_works() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 25::oid::regtype::text"), "text");
    assert_eq!(one(&mut e, "SELECT 23::oid::regtype::text"), "integer");
    // …and round-trips.
    assert_eq!(one(&mut e, "SELECT 25::oid::regtype::oid"), "25");
}

#[test]
fn round648_an_unknown_type_name_is_still_refused() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT 'nosuchtype'::regtype")
        .expect_err("PG raises 42704 here");
    assert!(
        err.to_string().contains("does not exist"),
        "unexpected message: {err}"
    );
}

/// The oid half has to be usable where an oid is usable — that is the
/// whole reason the dual shape exists.
#[test]
fn round648_regtype_joins_the_catalog_on_its_oid() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT typname FROM pg_type WHERE oid = 'text'::regtype::oid"
        ),
        "text"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_type WHERE oid = 'int4'::regtype"
        ),
        "1"
    );
}
