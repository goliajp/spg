//! v7.39 (round 263) — the COMPOSITE type surface, swept 55 cases
//! against live PG18.4 (2026-07-20). ROW construction, comparison,
//! ordering, the record text literal (quotes, embedded commas, empty
//! fields) and composite arrays already matched. The severe gap was
//! that a composite COLUMN did not round-trip at all:
//!
//! A composite column stores JSON keyed by field NAME, and those names
//! are PG-observable — `row_to_json(col)` keys by them (probed), which
//! is what settled that the fix belongs on the WRITE side rather than
//! papering over it on read. Two inputs reached the column without ever
//! being labelled by the target type:
//!   * `INSERT … VALUES (1, ROW('elm', 999))` stored the constructor's
//!     placeholder names `f1`/`f2`, so the read side — which looks
//!     fields up BY NAME — rebuilt an all-NULL record. `(elm,999)` came
//!     back as `(,)`: SILENT DATA LOSS.
//!   * a record TEXT literal was stored verbatim, which is not JSON, so
//!     the read side's parse failed and `(a).street` errored with
//!     "requires a composite (record) value" on a column that plainly
//!     held one.
//! Relabelling through the declared type also COERCES each field, which
//! is what now refuses `ROW('x','notanint')::addr` — that used to be
//! accepted with the text sitting in an int field.
//!
//! Also closed: `pg_typeof` over a composite CAST reported the generic
//! `record`, and the cast-arity refusal used SPG's own wording rather
//! than PG's `cannot cast type record to addr`.
//!
//! Recorded residuals, all probed: nested composite field access
//! (`((ROW(ROW('s',1)::addr,7)::nested).inner_a).street`); a subquery
//! whose result is a composite (`(SELECT ROW('a',1)::addr) IS NOT
//! NULL`); the field-not-found wording, which needs the base
//! expression's composite type resolved statically to say `column
//! "nosuch" not found in data type addr`; and `(1,2).f1`, which PG
//! rejects as a syntax error where SPG answers 1.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE paddr AS (street text, zip int)")
        .unwrap();
    e.execute("CREATE TABLE pcp (id int, a paddr)").unwrap();
    e.execute("INSERT INTO pcp VALUES (1, ROW('elm', 999))")
        .unwrap();
    e.execute("INSERT INTO pcp VALUES (2, '(\"oak ave\",111)')")
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

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn a_composite_column_round_trips_from_both_input_forms() {
    let mut e = seeded();
    // The ROW-inserted row used to come back as `(,)`.
    assert_eq!(
        rows_of(&mut e, "SELECT id, a FROM pcp ORDER BY id"),
        ["1|(elm,999)", "2|(\"oak ave\",111)"]
    );
    assert_eq!(
        one(&mut e, "SELECT a::text FROM pcp WHERE id=1"),
        "(elm,999)"
    );
    // Field access works on a column, not just on a cast literal.
    assert_eq!(
        rows_of(
            &mut e,
            "SELECT id, (a).street, (a).zip FROM pcp ORDER BY id"
        ),
        ["1|elm|999", "2|oak ave|111"]
    );
    assert_eq!(
        one(&mut e, "SELECT (a).zip + 1 FROM pcp WHERE id = 1"),
        "1000"
    );
    // The stored field NAMES are the declared ones, for both inputs.
    assert_eq!(
        rows_of(&mut e, "SELECT row_to_json(a) FROM pcp ORDER BY id"),
        [
            "{\"street\":\"elm\",\"zip\":999}",
            "{\"street\":\"oak ave\",\"zip\":111}"
        ]
    );
}

#[test]
fn casting_into_a_composite_coerces_each_field() {
    let mut e = seeded();
    // Used to be accepted, leaving the text in an int field.
    let got = err(&mut e, "SELECT ROW('x','notanint')::paddr");
    assert!(
        got.contains("invalid input syntax for type integer: \"notanint\""),
        "{got}"
    );
    // Shape mismatches take PG's plain cast refusal.
    for sql in ["SELECT ROW('x')::paddr", "SELECT ROW('x',1,2)::paddr"] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("cannot cast type record to paddr"),
            "{sql} → {got}"
        );
    }
}

#[test]
fn pg_typeof_names_the_composite() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT pg_typeof(a) FROM pcp LIMIT 1"), "paddr");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof('(\"main st\",12345)'::paddr)"),
        "paddr"
    );
    // The catalog gate still holds for builtin casts.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(1::float8)"),
        "double precision"
    );
}

#[test]
fn the_composite_core_is_unchanged() {
    let mut e = seeded();
    for (sql, want) in [
        ("SELECT ROW(1,2) = ROW(1,2)", "t"),
        ("SELECT ROW(1,2) < ROW(1,3)", "t"),
        ("SELECT ('(\"a\",)'::paddr).zip IS NULL", "t"),
        ("SELECT (ARRAY[ROW('a',1)::paddr])[1]", "(a,1)"),
        ("SELECT '(\"has,comma\",1)'::paddr", "(\"has,comma\",1)"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
