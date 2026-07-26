//! v7.39 (round 509) — a cast's TARGET is checked even when the value is NULL.
//!
//! `SELECT NULL::nosuchtype` answered NULL. `SELECT 1::nosuchtype` errored.
//! The gap was exactly the NULL operand, in both spellings (`::t` and
//! `CAST(… AS t)`), because the cast short-circuited on a NULL value before
//! it ever looked at what it was casting TO — so a misspelt type name
//! silently produced NULL, and `pg_typeof(NULL::nosuchtype)` answered
//! `unknown`. PG18 errors on all three.
//!
//! It surfaced from the function-surface sweep rather than from anyone
//! reading the code: every probe there is a `NULL::<type>` call, so on a
//! type SPG does not have, the NULL cast quietly degraded to `unknown` and
//! the error came back blaming the FUNCTION. The sweep's numbers could not
//! be trusted until this was fixed.
//!
//! PG's rule is that the type must EXIST; the conversion may still fail.
//! `NULL::mytable` is valid — a table names a row type — and `1::mytable`
//! is "cannot cast type integer to mytable". Both are pinned below.
//!
//! The check needs a catalog, since enums, domains, composite types and
//! table row types all live there, so it runs in `eval_cast_arm` rather than
//! in the cast itself. A context with no catalog keeps the old pass-through:
//! that is a stated limit, not an oversight — the alternative was a second
//! copy of the resolution rules, and the first cut of exactly that missed
//! `::binary`, table row types and the pseudotypes.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pr (a INT, b TEXT)").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// A type name that names nothing is an error whatever the operand is.
#[test]
fn round509_an_unknown_cast_target_errors_even_for_null() {
    let mut e = engine();
    for sql in [
        "SELECT NULL::nosuchtype",
        "SELECT CAST(NULL AS nosuchtype)",
        "SELECT pg_typeof(NULL::nosuchtype)",
        "SELECT NULL::totally_made_up_type",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("nosuchtype") || got.contains("totally_made_up_type"),
            "{sql}: expected the type name in the error, got {got}"
        );
    }
    // The value form was already right and stays right.
    assert!(err(&mut e, "SELECT 1::nosuchtype").contains("nosuchtype"));
}

/// Everything that DOES name a type still casts, which is the half a
/// careless fix breaks. Each of these resolves through a different door.
#[test]
fn round509_every_real_type_name_still_casts_null() {
    let mut e = engine();
    for sql in [
        "SELECT NULL::int4",            // the builtin table
        "SELECT NULL::timestamp(3)",    // a temporal precision
        "SELECT NULL::bit(4)",          // a bit width
        "SELECT NULL::anyarray",        // a pseudotype
        "SELECT NULL::record",          // the anonymous composite
        "SELECT NULL::pr",              // a table's row type
    ] {
        assert_eq!(text(&mut e, sql), "NULL", "{sql}");
    }
}

/// A table names a row type: the TYPE exists, so the cast target is fine and
/// only the conversion is refused — with PG's wording.
#[test]
fn round509_a_table_names_a_row_type() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT NULL::pr"), "NULL");
    let got = err(&mut e, "SELECT 1::pr");
    assert!(
        got.contains("cannot cast type integer to pr"),
        "expected PG's wording, got {got}"
    );
}

/// The spelling PG documents for "shaped like this table" keeps working —
/// it is a NULL cast to a row type, and it is what caught the first two
/// attempts at this fix.
#[test]
fn round509_populate_record_still_takes_its_shape_from_a_row_type() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT a FROM jsonb_populate_record(NULL::pr, '{\"a\":7}'::jsonb) AS t"
        ),
        "7"
    );
}
