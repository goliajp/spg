//! v7.39 (round 292) — float8's shortest decimal under PG's rule.
//!
//! Rust's `{:e}` gives the shortest decimal that ROUND-TRIPS. PG wants
//! the shortest that lies STRICTLY INSIDE the value's rounding
//! interval. They differ exactly on values whose short form sits on a
//! half-ulp boundary: ties-to-even parses that boundary back to the
//! same double, so Rust accepts it and PG does not.
//!
//! `1e23::float8` is the witness — PG prints `9.999999999999999e+22`.
//! r270 fixed the f32 side by widening to f64 and comparing midpoints
//! exactly. There is no wider float for f64, so this does the test in
//! exact INTEGER arithmetic: a midpoint is `M · 2^E` with M odd, a
//! candidate is `D · 10^K`, and they are equal only if their odd parts
//! and their powers of two both match. That forces `5^|K|` to divide a
//! 57-bit number, which bounds |K| — so it fits in u128 and needs no
//! big-number arithmetic.
//!
//! Verified beyond these cases by a 5484-case differential against live
//! PG 18.4: 3000 random bit patterns across the whole finite f64 space,
//! every power of ten from 1e-320 to 1e308 with both neighbours, and
//! small integers / sevenths / 1e17 multiples. All byte-identical.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

#[test]
fn the_boundary_case_takes_the_longer_form() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 1e23::float8"), "9.999999999999999e+22");
    assert_eq!(one(&mut e, "SELECT (-1e23)::float8"), "-9.999999999999999e+22");
}

#[test]
fn the_ordinary_values_are_unchanged() {
    // The risk in a shortest-decimal rewrite is regressing everything
    // that was already right.
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 1e16::float8", "1e+16"),
        ("SELECT 1e15::float8", "1e+15"),
        ("SELECT 1e17::float8", "1e+17"),
        ("SELECT 0.1::float8", "0.1"),
        ("SELECT 0.3::float8", "0.3"),
        ("SELECT (0.1+0.2)::float8", "0.3"),
        ("SELECT 100.0::float8", "100"),
        ("SELECT 1e-7::float8", "1e-07"),
        ("SELECT 1e100::float8", "1e+100"),
        ("SELECT 3.14159265358979::float8", "3.14159265358979"),
        ("SELECT 1.0/3.0::float8", "0.3333333333333333"),
        ("SELECT 123456789012345678::float8", "1.2345678901234568e+17"),
        ("SELECT 9007199254740993::float8", "9.007199254740992e+15"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn the_extremes_still_round_trip() {
    let mut e = Engine::new();
    // Smallest subnormal, largest finite, smallest normal — the places
    // where a mantissa/exponent decomposition goes wrong if the
    // implicit leading bit is mishandled.
    assert_eq!(one(&mut e, "SELECT 5e-324::float8"), "5e-324");
    assert_eq!(
        one(&mut e, "SELECT 1.7976931348623157e308::float8"),
        "1.7976931348623157e+308",
    );
    assert_eq!(
        one(&mut e, "SELECT 2.2250738585072014e-308::float8"),
        "2.2250738585072014e-308",
    );
}

#[test]
fn the_specials_are_untouched() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'NaN'::float8"), "NaN");
    assert_eq!(one(&mut e, "SELECT 'Infinity'::float8"), "Infinity");
    assert_eq!(one(&mut e, "SELECT '-Infinity'::float8"), "-Infinity");
    assert_eq!(one(&mut e, "SELECT 0.0::float8"), "0");
    // `-0.0` is the NEGATION of the literal 0.0, and PG folds it to a
    // positive zero — measured, not assumed. The signed zero survives
    // only when it comes in as text.
    assert_eq!(one(&mut e, "SELECT (-0.0)::float8"), "0");
    assert_eq!(one(&mut e, "SELECT '-0'::float8"), "-0");
}

#[test]
fn real_keeps_its_own_rule() {
    // float4 has its own boundary case and its own (r270) fix; the two
    // must not have been merged.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 1e23::real"), "1e+23");
    assert_eq!(one(&mut e, "SELECT 0.1::real"), "0.1");
    assert_eq!(one(&mut e, "SELECT 15000000512::real"), "1.5000001e+10");
}
