//! 9.0.0 — A9b: three of the four analysis-time errors.
//!
//! PostgreSQL resolves the statement before it scans, so these raise
//! whether or not the table holds a row. SPG resolved them per row, so
//! over an EMPTY table the query answered no rows and no error — and a
//! prepared statement DESCRIBED successfully, which is how an ORM
//! learns a query is valid.
//!
//! Measured on PostgreSQL 18.6 over an empty `rr9 (r real, id int)`:
//!
//! ```text
//!   SELECT nofn(id) FROM rr9              function nofn(integer) does not exist
//!   SELECT r + 'x' FROM rr9               invalid input syntax for type real: "x"
//!   SELECT count(*) FROM rr9 WHERE r = 'abc'
//!                                         invalid input syntax for type real: "abc"
//! ```
//!
//! The fourth — `SELECT lower(r)`, a KNOWN name with an argument type
//! no overload takes — is refused by SPG with PostgreSQL's exact
//! sentence, but only once a row reaches the call: answering it earlier
//! needs a per-function parameter-type table, and SPG has none
//! (`pg_proc.proargtypes` is empty and `eval/arity.rs` counts arguments
//! without typing them).
//!
//! The name check asks the DISPATCH which names it knows, through the
//! generated `KNOWN_FUNCTION_NAMES`. A9b's first attempt asked three
//! static lists and refused 346 real functions.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => alloc_message(&err),
        Ok(other) => panic!("{sql} was accepted: {other:?}"),
    }
}

fn alloc_message(e: &spg_engine::EngineError) -> String {
    format!("{e}")
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rr9 (r real, id int)")
        .expect("table");
    e
}

#[test]
fn an_unknown_function_is_refused_before_the_scan() {
    let mut e = seeded();
    assert!(
        err(&mut e, "SELECT nofn(id) FROM rr9").contains("function nofn(integer) does not exist"),
        "{}",
        err(&mut e, "SELECT nofn(id) FROM rr9")
    );
}

#[test]
fn a_literal_that_is_not_that_type_is_refused_before_the_scan() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "SELECT r + 'x' FROM rr9",
            "invalid input syntax for type real: \"x\"",
        ),
        (
            "SELECT count(*) FROM rr9 WHERE r = 'abc'",
            "invalid input syntax for type real: \"abc\"",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}: {got}");
    }
}

#[test]
fn a_function_the_dispatch_knows_is_not_refused() {
    // The direction that matters: over-refusal takes a working query
    // away. These are all real, and two of them are the ones A9b's
    // first attempt broke.
    let mut e = seeded();
    for sql in [
        "SELECT lower('A') FROM rr9",
        "SELECT abs(id) FROM rr9",
        "SELECT col_description(1, 1) FROM rr9",
        "SELECT area(id) FROM rr9",
        "SELECT count(*) FROM rr9",
        "SELECT r FROM rr9 WHERE upper('x') = 'X'",
        // A set-returning call never reaches the scalar dispatch, so
        // the probe that builds the table calls it unknown. 23 tests
        // caught this before a pin did.
        "SELECT unnest(ARRAY[1,2])",
        "SELECT generate_series(1, 3)",
        "SELECT jsonb_array_elements('[1,2]'::jsonb)",
    ] {
        match e.execute(sql) {
            Ok(_) => {}
            Err(err) => {
                let m = alloc_message(&err);
                assert!(
                    !m.contains("does not exist"),
                    "{sql} was refused as unknown: {m}"
                );
            }
        }
    }
}

#[test]
fn a_literal_that_IS_that_type_is_not_refused() {
    let mut e = seeded();
    e.execute("CREATE TABLE dd (d date, t text, n numeric)")
        .expect("table");
    for sql in [
        "SELECT count(*) FROM dd WHERE d = '2020-01-01'",
        "SELECT count(*) FROM dd WHERE t = 'anything at all'",
        "SELECT count(*) FROM dd WHERE n = '1.5'",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
}

#[test]
fn the_refusal_does_not_need_a_row() {
    // The whole point: an EMPTY table used to answer zero rows.
    let mut e = seeded();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT count(*) FROM rr9").expect("count runs")
    else {
        panic!("expected rows")
    };
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "0",
        "the fixture must be empty for this to mean anything"
    );
    assert!(err(&mut e, "SELECT nofn(id) FROM rr9").contains("does not exist"));
}
