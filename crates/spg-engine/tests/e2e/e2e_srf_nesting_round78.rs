//! v7.39 (read01 round 78) — a differential sweep of the string family. 43 of
//! its 44 probes were already identical; the one that was not turned out to be
//! the visible corner of something much larger.
//!
//!   `SELECT upper(unnest(ARRAY['a','b']))`  →  "unknown function unnest"
//!
//! A set-returning function was only ever recognised when it WAS the whole
//! target item. Wrapped in anything at all — a cast, a concatenation, an
//! arithmetic operator, another function — it fell through to the scalar
//! function dispatcher, which of course has no `unnest`. So the error did not
//! say "SRFs cannot be nested"; it said the function does not exist. The symptom
//! was two layers above the cause, again.
//!
//! PG's ProjectSet evaluates the SRF to a set and applies the enclosing
//! expression once per element. Each SRF node is now lifted out into a synthetic
//! column, the tree rewritten to read it, and the rewritten expression evaluated
//! per output row — so the nesting works for every enclosing shape rather than a
//! list of blessed ones. The lift carries VALUES, not literals: a text[] stays a
//! text[], a jsonb stays a jsonb.
//!
//! Doing that needed one exhaustive mutable walk over the expression tree
//! (`expr_analysis::rewrite_nodes_mut`). Every mutable rewrite in the engine had
//! been spelling out all ~25 Expr variants by hand.
//!
//! Two smaller PG rules fell out of the same probe:
//!   * A FROM item calling a function that returns a BASE type has that scalar as
//!     its row type, so a whole-row reference collapses to the value:
//!     `SELECT j FROM jsonb_array_elements('[1]') AS j` is `1`, not `(1)`. A
//!     one-column table or subquery does NOT collapse, so the parser marks which
//!     items it desugared from a function call.
//!   * An SRF inside CASE / COALESCE is an error, not a set: the set would have
//!     to be produced before anyone knows whether the branch is taken.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
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

fn r1(e: &mut Engine, sql: &str) -> String {
    rows_of(e, sql).join(",")
}

#[test]
fn a_srf_nested_in_an_expression() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT upper(unnest(ARRAY['a','b']))"), "A,B");
    assert_eq!(r1(&mut e, "SELECT unnest(ARRAY[1,2]) + 10"), "11,12");
    assert_eq!(r1(&mut e, "SELECT 'e:' || unnest(ARRAY['x','y'])"), "e:x,e:y");
    assert_eq!(r1(&mut e, "SELECT generate_series(1,2) * 2"), "2,4");
    // The SRF keeps its real type through the lift: text[] here, jsonb below.
    assert_eq!(
        r1(&mut e, "SELECT (regexp_matches('a1b2','([a-z])([0-9])','g'))::text"),
        "{a,1},{b,2}"
    );
    assert_eq!(r1(&mut e, "SELECT (jsonb_array_elements('[1,2]'::jsonb))::text"), "1,2");
    // Bare SRF items still behave, and an empty set is zero rows.
    assert_eq!(r1(&mut e, "SELECT unnest(ARRAY[1,2])"), "1,2");
    assert_eq!(rows_of(&mut e, "SELECT unnest('{}'::int[])").len(), 0);
}

#[test]
fn b_several_srfs_run_in_lockstep_after_nesting() {
    let mut e = Engine::new();
    // PG pads the shorter set with NULLs rather than taking a cross product.
    assert_eq!(
        r1(
            &mut e,
            "SELECT upper(unnest(ARRAY['a','b','c'])), generate_series(1,2) + 100"
        ),
        "A|101,B|102,C|NULL"
    );
}

#[test]
fn c_srf_in_a_conditional_is_an_error() {
    let mut e = Engine::new();
    // PG: "set-returning functions are not allowed in COALESCE".
    assert!(e.execute("SELECT coalesce(unnest(ARRAY['a',NULL]),'-')").is_err());
    assert!(
        e.execute("SELECT CASE WHEN true THEN unnest(ARRAY[1,2]) ELSE 0 END")
            .is_err()
    );
}

#[test]
fn d_scalar_function_from_item_has_the_scalar_as_its_row_type() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT j::text FROM jsonb_array_elements('[1,2]'::jsonb) AS j"), "1,2");
    assert_eq!(
        r1(&mut e, "SELECT m::text FROM regexp_matches('a1b2','([a-z])([0-9])','g') AS m"),
        "{a,1},{b,2}"
    );
    // The declared column name still works alongside the collapse.
    assert_eq!(
        r1(&mut e, "SELECT j.value::text FROM jsonb_array_elements('[1]'::jsonb) AS j"),
        "1"
    );
    // A one-column SUBQUERY is not a function item — it stays a composite.
    assert_eq!(r1(&mut e, "SELECT s::text FROM (SELECT 1 a) s"), "(1)");
}

#[test]
fn e_regexp_matches_as_a_from_item() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT m[1] || m[2] FROM regexp_matches('a1b2','([a-z])([0-9])','g') AS m"),
        "a1,b2"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT v::text, o FROM regexp_matches('a1b2','([a-z])([0-9])','g') \
             WITH ORDINALITY AS t(v, o)"
        ),
        "{a,1}|1,{b,2}|2"
    );
    assert_eq!(rows_of(&mut e, "SELECT * FROM regexp_matches('zzz','([0-9])','g')").len(), 0);
    // More column aliases than the item has columns is PG's error, reported here
    // rather than two layers down as "column not found".
    assert!(
        e.execute("SELECT * FROM regexp_matches('a1','([a-z])([0-9])') AS t(a, b)")
            .is_err()
    );
}
