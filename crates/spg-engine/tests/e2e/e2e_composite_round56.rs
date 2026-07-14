//! v7.39 (read01 dir-outside, round 56) — a user-defined COMPOSITE column is a
//! real composite VALUE, not opaque JSON.
//!
//! SPG stores a composite column as JSONB, and `Value::Composite` (field access,
//! canonical `(2,b)` text, ROW comparison, ordering) has existed since round 28 —
//! but the two were never wired together: `ColumnSchema` carried no
//! `user_composite_type` marker (it lived in a doc comment only), so a column
//! never knew which composite type it held and the stored JSON stayed JSON. The
//! old e2e_composite_type only ever inserted a JSON literal and read `id` back,
//! which is why the hole went unnoticed for two releases.
//!
//! The marker now rides a v63 tail appendix, and `resolve_column` rehydrates the
//! stored JSON into a `Value::Composite` in the catalog's declared field order.
//! Every expectation below is byte-locked against a live PG18.4 oracle.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE pt AS (x int, y text)");
    ok(&mut e, "CREATE TABLE cp (id int, p pt)");
    ok(
        &mut e,
        "INSERT INTO cp VALUES (1, ROW(2,'b')::pt), (2, ROW(9,'a')::pt)",
    );
    e
}

#[test]
fn field_access_keeps_the_declared_field_type() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT (p).x FROM cp WHERE id = 1"), "2");
    assert_eq!(r1(&mut e, "SELECT (p).y FROM cp WHERE id = 1"), "b");
    // `+ 10` is integer math — the JSON field is coerced back to its DECLARED
    // type on rehydration, so this is 12 and not a string concatenation.
    assert_eq!(r1(&mut e, "SELECT (p).x + 10 FROM cp WHERE id = 1"), "12");
}

#[test]
fn composite_renders_in_canonical_paren_form() {
    let mut e = seeded();
    // `(2,b)` — NOT the `{"x":2,"y":"b"}` the storage form would give.
    assert_eq!(r1(&mut e, "SELECT p::text FROM cp WHERE id = 1"), "(2,b)");
}

#[test]
fn whole_composite_compares_against_a_row_literal() {
    let mut e = seeded();
    // This is the one that needed a gate in the v7.32 P4 borrow channel: the
    // comparison fast path hands back a raw reference into the row, so a
    // composite column has to opt OUT of borrowing (rehydration builds a NEW
    // value that does not live in the row).
    assert_eq!(
        col(&mut e, "SELECT id FROM cp WHERE p = ROW(2,'b')::pt"),
        ["1"]
    );
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM cp WHERE p <> ROW(2,'b')::pt"),
        "1"
    );
}

#[test]
fn composite_orders_field_by_field() {
    let mut e = seeded();
    // (2,b) < (9,a) — the leading field decides, so id 1 comes first.
    assert_eq!(col(&mut e, "SELECT id FROM cp ORDER BY p"), ["1", "2"]);
    assert_eq!(col(&mut e, "SELECT id FROM cp ORDER BY p DESC"), ["2", "1"]);
}

#[test]
fn composite_becomes_a_json_object() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT row_to_json(p) FROM cp WHERE id = 1"),
        r#"{"x":2,"y":"b"}"#
    );
}

#[test]
fn pg_typeof_reports_the_composite_type_name() {
    let mut e = seeded();
    // Composite-ness lives OUTSIDE the DataType lattice (the value's storage
    // form is JSON), so this rides a static witness off the column marker —
    // the same shape as the enum witness. PG says `pt`, not `record`.
    assert_eq!(r1(&mut e, "SELECT pg_typeof(p) FROM cp WHERE id = 1"), "pt");
}

#[test]
fn update_writes_a_composite_back_out() {
    let mut e = seeded();
    ok(&mut e, "UPDATE cp SET p = ROW(7,'z')::pt WHERE id = 1");
    assert_eq!(r1(&mut e, "SELECT (p).x FROM cp WHERE id = 1"), "7");
    assert_eq!(r1(&mut e, "SELECT p::text FROM cp WHERE id = 1"), "(7,z)");
    assert_eq!(
        col(&mut e, "SELECT id FROM cp WHERE p = ROW(7,'z')::pt"),
        ["1"]
    );
}
