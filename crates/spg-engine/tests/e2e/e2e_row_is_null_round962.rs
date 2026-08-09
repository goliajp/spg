//! r962 — `IS [NOT] NULL` on a row VALUE, not just the `ROW(...)` spelling.
//!
//! Round P4.11 taught the field-wise rule — a row IS NULL when every
//! field is null, IS NOT NULL when every field is non-null, so the two
//! are not negations of each other and `ROW(1,NULL)` is neither. It keyed
//! on the SYNTAX, so every other way to hold a row kept the whole-value
//! test: a column declared with a composite type, and (once round 961
//! made them reachable through a projection) a whole-row reference.
//!
//! Measured against PG18.4 over the wire, round 962. Every expectation
//! below is that server's answer.

use spg_engine::{CancelToken, Engine, StreamItem};

fn one(e: &Engine, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item {
            out.push(format!("{:?}", cells.get(0).expect("a first cell")));
        }
        Ok(())
    })
    .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    out
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn engine_with_an() -> Engine {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE an (id INT, q TEXT)");
    // Row 1: every column NULL. Row 2: mixed.
    run(&mut e, "INSERT INTO an VALUES (NULL, NULL), (1, NULL)");
    e
}

#[test]
fn a_whole_row_reference_uses_the_field_wise_rule() {
    let e = engine_with_an();
    assert_eq!(
        one(&e, "SELECT an IS NULL FROM an"),
        vec!["Bool(true)".to_string(), "Bool(false)".to_string()],
        "all fields null -> true; one non-null field -> false"
    );
    // NOT the negation: the mixed row is neither IS NULL nor IS NOT NULL.
    assert_eq!(
        one(&e, "SELECT an IS NOT NULL FROM an"),
        vec!["Bool(false)".to_string(), "Bool(false)".to_string()],
        "IS NOT NULL needs EVERY field non-null"
    );
}

#[test]
fn a_composite_typed_column_uses_the_field_wise_rule() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TYPE pt AS (x INT, y INT)");
    run(&mut e, "CREATE TABLE c (id INT, p pt)");
    run(&mut e, "INSERT INTO c VALUES (1, ROW(NULL, NULL)::pt)");
    run(&mut e, "INSERT INTO c VALUES (2, ROW(1, NULL)::pt)");

    assert_eq!(
        one(&e, "SELECT p IS NULL FROM c"),
        vec!["Bool(true)".to_string(), "Bool(false)".to_string()]
    );
    assert_eq!(
        one(&e, "SELECT p IS NOT NULL FROM c"),
        vec!["Bool(false)".to_string(), "Bool(false)".to_string()]
    );
}

#[test]
fn the_row_spelling_still_answers_as_it_did() {
    let e = Engine::new();
    // The P4.11 cases, unchanged — the value-side rule must not have
    // shifted the syntax-side one.
    assert_eq!(one(&e, "SELECT ROW(NULL,NULL) IS NULL"), ["Bool(true)"]);
    assert_eq!(one(&e, "SELECT ROW(1,NULL) IS NULL"), ["Bool(false)"]);
    assert_eq!(one(&e, "SELECT ROW(1,NULL) IS NOT NULL"), ["Bool(false)"]);
    assert_eq!(
        one(&e, "SELECT ROW(NULL,NULL) IS NOT NULL"),
        ["Bool(false)"]
    );
    // A field that is itself a row counts as a non-null value; the rule
    // does not recurse.
    assert_eq!(
        one(&e, "SELECT ROW(ROW(NULL,NULL)) IS NULL"),
        ["Bool(false)"]
    );
}

#[test]
fn a_scalar_is_unaffected() {
    let e = Engine::new();
    assert_eq!(one(&e, "SELECT NULL::int IS NULL"), ["Bool(true)"]);
    assert_eq!(one(&e, "SELECT 1 IS NULL"), ["Bool(false)"]);
    assert_eq!(one(&e, "SELECT NULL::int IS NOT NULL"), ["Bool(false)"]);
    assert_eq!(one(&e, "SELECT 1 IS NOT NULL"), ["Bool(true)"]);
}
