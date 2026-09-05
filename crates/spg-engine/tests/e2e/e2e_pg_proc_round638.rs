//! v7.39 (round 638, F20) — pg_proc listed 90 of the functions the engine
//! has.
//!
//! PG has 3415 rows. SPG had 90, so a tool asking "does this function
//! exist" was told no for almost everything the engine can actually do.
//! It has 373 now, 338 distinct names.
//!
//! Every row is measured, not declared. Rounds 636 and 637 probed the
//! dispatcher's 1148 names against the engine; the arity comes out of its
//! own "takes N arg" errors and the return type from
//! `pg_typeof(f(NULL...))`, with typed NULLs where an untyped one left the
//! answer unknown. Only the 283 whose return type could actually be
//! measured were added — the rest are stubs whose result is an untyped
//! NULL, and giving them a made-up type would invent the very thing this
//! catalog exists to report.
//!
//! `pronargs` comes from PG where PG has the function: that is the
//! signature the implementation targets, and SPG's stubs accept any arity,
//! so its own arity probe is not evidence of the intended one. For the 86
//! functions PG does not have, the measured arity is all there is.
//!
//! And the canonical join found the next gap again, as it did for pg_cast
//! last round: 18 of the 373 rows pointed at types pg_type never listed.
//! Four of those orphans predate this round — `array_agg`, `lag`, `lead`,
//! `first_value` and `last_value` have always returned anyelement or
//! anyarray, and neither type was there. Ten types added, read off PG18.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn round638_pg_proc_lists_what_the_engine_has() {
    let mut e = Engine::new();
    // v7.40.0 — 881 became 882: `isnull`, MySQL's one-argument NULL
    // test, joined the catalog. The count is checked because the
    // catalog is hand-kept and a function that exists but is not listed
    // is invisible to every tool that reads it.
    // v7.39 (round 653) — 373/338 became 716/573. The old numbers were not
    // wrong when written; they were the size of a catalog that listed 338 of
    // the 709 functions the engine answers. The gap was measured by calling
    // every candidate name and reading the engine's own reply, which
    // separates "does not exist" from "takes N args".
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pg_proc"), vec!["882"]);
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT proname) FROM pg_proc"),
        vec!["574"]
    );
    // Signatures byte for byte with PG18's for the same names.
    assert_eq!(
        vals(
            &mut e,
            "SELECT p.proname, p.pronargs, t.typname FROM pg_proc p \
             JOIN pg_type t ON t.oid = p.prorettype \
             WHERE p.proname IN ('acos','ascii','char_length','sqrt') ORDER BY 1, 3"
        ),
        // v7.39 (round 654) — `char_length` gained its second row when the
        // overload layer landed; PG18 has two (text and bpchar) and now so
        // does SPG.
        vec![
            "acos|1|float8",
            "ascii|1|int4",
            "char_length|1|int4",
            "char_length|1|int4",
            "sqrt|1|float8",
            "sqrt|1|numeric"
        ]
    );
}

/// Every row's return type resolves — the check that found the last gap.
#[test]
fn round638_no_row_is_orphaned_by_the_join() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_proc p JOIN pg_type t ON t.oid = p.prorettype"
        ),
        vec!["882"],
        "as many as pg_proc has — nothing points at a type pg_type omits"
    );
}

/// The types that join needed, including four that were orphaned before
/// this round added anything.
#[test]
fn round638_pg_type_lists_the_pseudo_and_multirange_types() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT typname, oid, typtype FROM pg_type \
             WHERE typname IN ('anyelement','anyarray','record','regtype') ORDER BY 1"
        ),
        vec![
            "anyarray|2277|p",
            "anyelement|2283|p",
            "record|2249|p",
            "regtype|2206|b"
        ]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM pg_type WHERE typtype = 'm'"),
        vec!["6"],
        "the multirange family"
    );
    // The functions that were orphaned resolve now.
    assert_eq!(
        vals(
            &mut e,
            "SELECT p.proname, t.typname FROM pg_proc p JOIN pg_type t ON t.oid = p.prorettype \
             WHERE p.proname IN ('array_agg','lag','first_value') ORDER BY 1, 2 LIMIT 3"
        ),
        // v7.39 (round 654) — PG18 carries `array_agg|anyarray` TWICE, so
        // under `ORDER BY 1, 2 LIMIT 3` its first three are the two
        // array_aggs and first_value. SPG matches that now; the old
        // expectation was the shape from before the overload layer.
        vec![
            "array_agg|anyarray",
            "array_agg|anyarray",
            "first_value|anyelement"
        ]
    );
}
