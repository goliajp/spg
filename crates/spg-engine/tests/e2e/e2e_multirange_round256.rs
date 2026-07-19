//! v7.39 (round 256) — the range / multirange surface, swept 148 cases
//! against live PG18.4 (2026-07-19). The range side was already solid
//! (67/68 straight through on the first pass: constructors, bound
//! flags, canonicalization, the whole operator family, lower/upper and
//! the inclusivity predicates). The gaps were all on the multirange
//! side and in one error wording:
//!
//!   * MIXED range↔multirange operands were unwired. `range &&
//!     multirange` fell through to the INET reading of `&&` and
//!     errored; `range @> multirange` and `multirange <@ range`
//!     answered a silent FALSE. PG treats a plain range as the
//!     one-element multirange containing it, so promoting once at the
//!     operator entry fixes every combination at once.
//!   * `<<` / `>>` / `&<` / `&>` / `-|-` had no multirange arms at all.
//!     PG reads a multirange as its outer HULL for these — pinned by a
//!     discriminating probe, not assumed: `{[1,3),[9,11)} -|- {[3,5)}`
//!     is FALSE even though the first element is adjacent to the
//!     operand, because the hull `[1,11)` overlaps it. An any-element
//!     rule would have said TRUE. An empty multirange answers false.
//!   * `pg_typeof` reported `unknown` for every multirange type.
//!   * The polymorphic `multirange(range)` constructor and the
//!     `range::<type>multirange` cast were missing.
//!   * `range @> array` answered a silent FALSE; PG has no such
//!     operator.
//!   * A range literal whose STRUCTURE parses but whose bound is not a
//!     value of the element type reported "malformed range literal";
//!     PG reserves that for a structural problem and reports the
//!     element's own error (`invalid input syntax for type integer`).

use spg_engine::{Engine, QueryResult};

/// Renders the way the differential oracle does: `psql -tA` prints a
/// boolean as `t` / `f`, where `value_to_text` spells it out. Every
/// expectation below is a probed psql value, so the pin has to speak
/// the same dialect.
fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
            other => spg_engine::eval::value_to_text(other),
        },
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
fn mixed_range_and_multirange_operands_resolve() {
    let mut e = Engine::new();
    for (sql, want) in [
        // && in every operand combination (the mixed ones errored).
        ("SELECT int4multirange(int4range(1,5), int4range(7,9)) && int4range(4,8)", "t"),
        ("SELECT int4range(4,8) && int4multirange(int4range(1,5))", "t"),
        ("SELECT int4multirange(int4range(1,5)) && int4multirange(int4range(4,9))", "t"),
        // Containment across the mix (these answered a silent false).
        ("SELECT int4range(1,9) @> int4multirange(int4range(2,3))", "t"),
        ("SELECT int4multirange(int4range(2,3)) <@ int4range(1,9)", "t"),
        ("SELECT int4multirange(int4range(1,5)) @> int4range(2,3)", "t"),
        ("SELECT int4multirange(int4range(1,5)) @> int4multirange(int4range(2,3))", "t"),
        // Adjacency across the mix.
        ("SELECT int4multirange(int4range(1,3)) -|- int4range(3,5)", "t"),
        ("SELECT int4range(1,3) -|- int4multirange(int4range(3,5))", "t"),
        // Positional across the mix.
        ("SELECT int4multirange(int4range(1,3), int4range(9,11)) << int4range(20,25)", "t"),
        ("SELECT int4range(1,3) << int4multirange(int4range(9,11), int4range(20,25))", "t"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn positional_operators_read_a_multirange_as_its_hull() {
    let mut e = Engine::new();
    for (sql, want) in [
        // THE discriminating case: an any-element rule would say `t`
        // (the first element is adjacent), the hull rule says `f`.
        ("SELECT int4multirange(int4range(1,3), int4range(9,11)) -|- int4multirange(int4range(3,5))", "f"),
        ("SELECT int4multirange(int4range(1,3), int4range(5,7)) -|- int4multirange(int4range(7,9))", "t"),
        ("SELECT int4multirange(int4range(1,3), int4range(5,7)) << int4multirange(int4range(9,11))", "t"),
        ("SELECT int4multirange(int4range(1,3), int4range(9,11)) << int4multirange(int4range(5,7))", "f"),
        ("SELECT int4multirange(int4range(6,9)) >> int4multirange(int4range(1,3))", "t"),
        ("SELECT int4multirange(int4range(1,3), int4range(9,11)) &< int4multirange(int4range(5,20))", "t"),
        ("SELECT int4multirange(int4range(1,3), int4range(9,11)) &> int4multirange(int4range(0,5))", "t"),
        // An empty multirange has no hull: false, never an error.
        ("SELECT int4multirange() << int4multirange(int4range(1,5))", "f"),
        ("SELECT int4multirange() -|- int4multirange(int4range(1,5))", "f"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn multirange_types_are_named_and_constructible() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT pg_typeof(int4multirange(int4range(1,5)))", "int4multirange"),
        ("SELECT pg_typeof(nummultirange(numrange(1,5)))", "nummultirange"),
        ("SELECT pg_typeof(datemultirange(daterange('2024-01-01','2024-02-01')))", "datemultirange"),
        // The polymorphic constructor takes its kind from the argument.
        ("SELECT multirange(int4range(1,5))", "{[1,5)}"),
        ("SELECT multirange(numrange(1,5))", "{[1,5)}"),
        ("SELECT pg_typeof(multirange(numrange(1,5)))", "nummultirange"),
        // …and the cast is the same promotion.
        ("SELECT int4range(1,5)::int4multirange", "{[1,5)}"),
        ("SELECT 'empty'::int4range::int4multirange", "{}"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // range_agg's result type is named too.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(range_agg(r)) FROM (VALUES (int4range(1,3))) t(r)"
        ),
        "int4multirange"
    );
}

#[test]
fn range_literal_errors_separate_structure_from_element() {
    let mut e = Engine::new();
    // Structure is fine, the BOUND is not of the element type — PG
    // reports the element's own input error, per element type.
    for (sql, want) in [
        ("SELECT '[a,b)'::int4range", "invalid input syntax for type integer: \"a\""),
        ("SELECT '[1,x)'::int8range", "invalid input syntax for type bigint: \"x\""),
        ("SELECT '[q,2)'::numrange", "invalid input syntax for type numeric: \"q\""),
        (
            "SELECT '[zzz,2024-01-01)'::daterange",
            "invalid input syntax for type date: \"zzz\"",
        ),
        (
            "SELECT '[nope,2024-01-01 00:00)'::tsrange",
            "invalid input syntax for type timestamp: \"nope\"",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
    // Structural problems keep "malformed range literal".
    for sql in [
        "SELECT '[1,2'::int4range",
        "SELECT '1,2)'::int4range",
        "SELECT '[1;2)'::int4range",
        "SELECT ''::int4range",
        "SELECT 'x'::int4range",
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains("malformed range literal"), "{sql} → {got}");
    }
    // …and the ordering check is its own message.
    let got = err(&mut e, "SELECT '[2,1)'::int4range");
    assert!(
        got.contains("range lower bound must be less than or equal to range upper bound"),
        "{got}"
    );
}

#[test]
fn range_contains_rejects_an_array_operand() {
    let mut e = Engine::new();
    // Answered a silent `f` before.
    let got = err(&mut e, "SELECT int4range(1,5) @> ARRAY[1,2]");
    assert!(
        got.contains("operator does not exist: int4range @> integer[]"),
        "{got}"
    );
    // The element and range forms still work.
    assert_eq!(one(&mut e, "SELECT int4range(1,10) @> 5"), "t");
    assert_eq!(one(&mut e, "SELECT int4range(1,10) @> int4range(2,5)"), "t");
}

#[test]
fn the_range_core_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT int4range(1,5,'[]')", "[1,6)"),
        ("SELECT int4range(1,5,'()')", "[2,5)"),
        ("SELECT int4range(1,1)", "empty"),
        ("SELECT int4range(5,1)", "ERR"),
        ("SELECT lower(int4range(1,5,'()'))", "2"),
        ("SELECT upper(int4range(1,5,'[]'))", "6"),
        ("SELECT isempty(int4range(1,1))", "t"),
        ("SELECT int4range(1,5) -|- int4range(5,10)", "t"),
        ("SELECT int4range(1,5) + int4range(4,10)", "[1,10)"),
        ("SELECT int4range(1,5) * int4range(4,10)", "[4,5)"),
        ("SELECT int4range(1,10) - int4range(5,20)", "[1,5)"),
        ("SELECT range_merge(int4range(1,3), int4range(6,9))", "[1,9)"),
        ("SELECT int4multirange(int4range(1,5)) + int4multirange(int4range(7,9))", "{[1,5),[7,9)}"),
        ("SELECT int4multirange(int4range(1,9)) - int4multirange(int4range(3,5))", "{[1,3),[5,9)}"),
        ("SELECT int4multirange(int4range(1,9)) * int4multirange(int4range(3,5))", "{[3,5)}"),
        ("SELECT range_merge(int4multirange(int4range(1,3), int4range(6,9)))", "[1,9)"),
    ] {
        if want == "ERR" {
            let got = err(&mut e, sql);
            assert!(
                got.contains("range lower bound must be less than or equal to range upper bound"),
                "{sql} → {got}"
            );
        } else {
            assert_eq!(one(&mut e, sql), want, "{sql}");
        }
    }
}
