//! v7.39 (round 270) — float4's shortest decimal, and the range errors
//! at both ends of both float widths.
//!
//! Rust renders the shortest decimal that ROUND-TRIPS. PG renders the
//! shortest that lies STRICTLY INSIDE the value's rounding interval.
//! The two differ exactly on values whose short form sits on a half-ulp
//! boundary — ties-to-even parses that boundary back to the same float,
//! so Rust accepts it and PG does not.
//!
//! Everything here was read off live PG 18.4; the rendering rule was
//! established by comparing 373 float4 values, of which one diverged.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    assert_eq!(rows.len(), 1, "{sql}");
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(x) => format!("{x}").replace("eval: type mismatch: ", ""),
    }
}

#[test]
fn a_value_on_a_half_ulp_boundary_takes_the_longer_form() {
    let mut e = Engine::new();
    // 1.5e10 is EXACTLY half an ulp below the float4 15000000512, so it
    // round-trips (ties-to-even) but is not strictly inside the
    // interval. PG prints the longer form; SPG printed 1.5e+10.
    assert_eq!(
        one(&mut e, "SELECT '15000000512'::real::text"),
        "1.5000001e+10"
    );
    assert_eq!(one(&mut e, "SELECT '1.5e10'::real::text"), "1.5000001e+10");
    // Both texts still name the same float4.
    assert_eq!(
        one(&mut e, "SELECT '1.5e10'::real = 15000000512::real"),
        "true"
    );
}

#[test]
fn ordinary_values_keep_their_short_form() {
    let mut e = Engine::new();
    // The rule must not lengthen anything else: these are the shortest
    // form in both engines.
    for (sql, want) in [
        ("SELECT '0.1'::real::text", "0.1"),
        ("SELECT '0.5'::real::text", "0.5"),
        ("SELECT '1e6'::real::text", "1e+06"),
        ("SELECT '16777216'::real::text", "1.6777216e+07"),
        ("SELECT '123456792'::real::text", "1.2345679e+08"),
        ("SELECT '3.4028235e38'::real::text", "3.4028235e+38"),
        ("SELECT '1.1754944e-38'::real::text", "1.1754944e-38"),
        ("SELECT '1.4e-45'::real::text", "1e-45"),
        ("SELECT '0.0001'::real::text", "0.0001"),
        ("SELECT '99999'::real::text", "99999"),
        ("SELECT '-1234.5'::real::text", "-1234.5"),
        ("SELECT '1e15'::real::text", "1e+15"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn underflowing_to_zero_is_an_error_not_a_silent_zero() {
    let mut e = Engine::new();
    // The bottom end of the range PG rejects. 7.1e-46 rounds up to the
    // smallest subnormal and is fine; 7.0e-46 rounds to zero and is not.
    assert_eq!(one(&mut e, "SELECT '7.1e-46'::real::text"), "1e-45");
    assert_eq!(
        err(&mut e, "SELECT '7.0e-46'::real"),
        "\"7.0e-46\" is out of range for type real",
    );
    assert_eq!(
        err(&mut e, "SELECT '1e-50'::real"),
        "\"1e-50\" is out of range for type real",
    );
    assert_eq!(
        err(&mut e, "SELECT '-1e-50'::real"),
        "\"-1e-50\" is out of range for type real",
    );
    // A source that really is zero stays zero.
    assert_eq!(one(&mut e, "SELECT '0'::real::text"), "0");
    assert_eq!(one(&mut e, "SELECT '0.0e-50'::real::text"), "0");
}

#[test]
fn narrowing_a_double_names_which_end_it_left() {
    let mut e = Engine::new();
    // PG words the two ends of a narrowing differently, and neither
    // quotes a source (there is no text to quote).
    assert_eq!(
        err(&mut e, "SELECT 1e40::float8::real"),
        "value out of range: overflow",
    );
    assert_eq!(
        err(&mut e, "SELECT 1e-50::float8::real"),
        "value out of range: underflow",
    );
    // A double that lands inside the subnormals is fine.
    assert_eq!(one(&mut e, "SELECT '1e-45'::float8::real::text"), "1e-45");
}

#[test]
fn a_numeric_literal_out_of_real_range_quotes_its_expansion() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 1e-50::real"),
        "\"0.00000000000000000000000000000000000000000000000001\" is out of range for type real",
    );
    assert_eq!(
        err(&mut e, "SELECT 1.0e40::real"),
        "\"10000000000000000000000000000000000000000\" is out of range for type real",
    );
}

#[test]
fn double_precision_range_text_is_out_of_range_not_bad_syntax() {
    let mut e = Engine::new();
    // These reported "invalid input syntax for type double precision",
    // which says the text is malformed. It is well-formed and simply
    // outside the range, which is what PG says.
    assert_eq!(
        err(&mut e, "SELECT '1e400'::float8"),
        "\"1e400\" is out of range for type double precision",
    );
    assert_eq!(
        err(&mut e, "SELECT '1e-400'::float8"),
        "\"1e-400\" is out of range for type double precision",
    );
    // Genuinely malformed text keeps the syntax wording.
    assert!(
        err(&mut e, "SELECT 'abc'::float8").contains("invalid input syntax"),
        "{}",
        err(&mut e, "SELECT 'abc'::float8"),
    );
    // In range, unchanged.
    assert_eq!(one(&mut e, "SELECT '1e-320'::float8::text"), "1e-320");
}
