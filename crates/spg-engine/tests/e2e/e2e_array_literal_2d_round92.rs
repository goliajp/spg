//! v7.39 (read01 round 92) — 2-D array LITERAL parsing (the pg_dump text form).
//!
//! `ARRAY[[1,2],[3,4]]` (the constructor) made 2-D values since round 75, but
//! the text-literal cast `'{{1,2},{3,4}}'::int[]` — the form pg_dump emits for
//! a 2-D array column — never learned nested braces: it stripped the outer
//! `{…}` and split the whole thing on the first comma, so `{1` came out as an
//! element and the cast errored (or, for text[], mis-split into `{a`, `b}`, …).
//! A table with a 2-D array column, dumped and restored, would fail to load.
//!
//! int / bigint / text / bool 2-D literals are now parsed depth-aware and folded
//! into the matching 2-D value. Two INSERT/cast error messages were aligned to
//! PG at the same time: a bad element is `invalid input syntax for type <T>:
//! "<v>"` and an unterminated literal is `malformed array literal: "<v>"`.
//!
//! (numeric/float/date 2-D literals still error — those element types have no
//! 2-D value variant yet — and 3-D+ is unrepresentable; both deferred.)

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    e.execute(sql).unwrap_err().to_string()
}

#[test]
fn a_2d_literals_parse_for_the_variant_types() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT ('{{1,2},{3,4}}'::int[])::text"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        r1(&mut e, "SELECT ('{{1,2},{3,4}}'::bigint[])::text"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        r1(&mut e, "SELECT ('{{a,b},{c,d}}'::text[])::text"),
        "{{a,b},{c,d}}"
    );
    assert_eq!(
        r1(&mut e, "SELECT ('{{t,f},{f,t}}'::bool[])::text"),
        "{{t,f},{f,t}}"
    );
    // Dimensions and subscripting work on the parsed 2-D value.
    assert_eq!(
        r1(&mut e, "SELECT array_dims('{{1,2},{3,4}}'::int[])"),
        "[1:2][1:2]"
    );
    assert_eq!(
        r1(&mut e, "SELECT ('{{1,2},{3,4}}'::int[])[1][2]::text"),
        "2"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_length('{{1,2,3},{4,5,6}}'::int[], 2)::text"
        ),
        "3"
    );
}

#[test]
fn b_1d_literals_unaffected() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT ('{1,2,3}'::int[])::text"), "{1,2,3}");
    assert_eq!(r1(&mut e, "SELECT ('{a,b,c}'::text[])::text"), "{a,b,c}");
    assert_eq!(r1(&mut e, "SELECT ('{t,f}'::bool[])::text"), "{t,f}");
    // A 1-D literal still round-trips through an INSERT into a plain array
    // column. (Storing a 2-D literal into a 1-D-declared `int[]` column needs
    // dimensionless-array storage support and is deferred.)
    e.execute("CREATE TABLE t (m int[])").unwrap();
    e.execute("INSERT INTO t VALUES ('{1,2,3}')").unwrap();
    assert_eq!(r1(&mut e, "SELECT m::text FROM t"), "{1,2,3}");
}

#[test]
fn c_array_element_and_literal_error_wording() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT '{1,notint,3}'::int[]")
            .contains("invalid input syntax for type integer: \"notint\"")
    );
    assert!(err(&mut e, "SELECT '{1,2'::int[]").contains("malformed array literal: \"{1,2\""));
}
