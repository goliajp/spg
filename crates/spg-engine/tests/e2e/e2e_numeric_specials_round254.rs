//! v7.39 (round 254) — the scalar math / NUMERIC surface, swept 196
//! cases against live PG18.4 (2026-07-19). The ordinary arithmetic
//! (rounding modes, div/mod signs, gcd/lcm, width_bucket, the integer
//! and float8 overflow errors) already matched; the gap was structural:
//!
//! `NumericKind` has existed since v7.38 and the comparison / min-max /
//! power paths honour it, but EVERY other math function and numeric
//! cast rebuilt its result with `kind: Finite`. A NaN or ±Infinity
//! argument therefore collapsed to the special's canonical mantissa 0
//! and the answer was silently wrong — `abs('-Infinity')` = 0,
//! `'Infinity'::numeric::float8` = 0, `round('NaN')` = 0.
//!
//! The fix is one shared table (`numeric::special_math`) consulted
//! ahead of the function dispatch, plus a cast pre-block for both
//! directions, rather than patching fifteen match arms. Every cell
//! below was probed, none inferred.
//!
//! Also closed here:
//!   * `power(0, -1)` answered Infinity for int / bigint / smallint /
//!     float8 and the `^` operator — the zero-base check sat BELOW the
//!     integer-exponent fast path, so only the NUMERIC overload reached
//!     it. PG raises for the whole tower.
//!   * `div()` answered integer; PG declares only `div(numeric,
//!     numeric)` and reports numeric (same digits, different pg_typeof).
//!   * `div(): ` / `mod(): ` prefixes leaked into PG's bare "division by
//!     zero", and an unknown function reported SPG's internal wording
//!     instead of PG's `function nosuchfn(integer) does not exist`.
//!
//! Probe-infrastructure lesson (r248's, repeated): the probe's own
//! NUMERIC renderer ignored `kind`, so the first sweep read 12 phantom
//! diffs. It now defers to the engine's renderer. Separately, piping a
//! multi-statement psql script with `2>&1` lets stderr and stdout
//! interleave out of order — the oracle is now one psql call per
//! statement.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
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
fn specials_survive_the_unary_math_family() {
    let mut e = Engine::new();
    for (sql, want) in [
        // abs folds -Infinity onto +Infinity.
        ("SELECT abs('NaN'::numeric)", "NaN"),
        ("SELECT abs('Infinity'::numeric)", "Infinity"),
        ("SELECT abs('-Infinity'::numeric)", "Infinity"),
        // The rounding family passes specials through, both spellings.
        ("SELECT trunc('NaN'::numeric)", "NaN"),
        ("SELECT trunc('-Infinity'::numeric)", "-Infinity"),
        ("SELECT trunc('Infinity'::numeric, 2)", "Infinity"),
        ("SELECT round('Infinity'::numeric)", "Infinity"),
        ("SELECT round('-Infinity'::numeric)", "-Infinity"),
        ("SELECT round('NaN'::numeric, 2)", "NaN"),
        ("SELECT ceil('NaN'::numeric)", "NaN"),
        ("SELECT ceil('Infinity'::numeric)", "Infinity"),
        ("SELECT ceil('-Infinity'::numeric)", "-Infinity"),
        ("SELECT floor('Infinity'::numeric)", "Infinity"),
        ("SELECT floor('-Infinity'::numeric)", "-Infinity"),
        ("SELECT floor('NaN'::numeric)", "NaN"),
        ("SELECT trim_scale('NaN'::numeric)", "NaN"),
        // sign reports an infinity's direction.
        ("SELECT sign('NaN'::numeric)", "NaN"),
        ("SELECT sign('Infinity'::numeric)", "1"),
        ("SELECT sign('-Infinity'::numeric)", "-1"),
        // Transcendentals: exp(-Infinity) is the one finite answer.
        ("SELECT sqrt('Infinity'::numeric)", "Infinity"),
        ("SELECT sqrt('NaN'::numeric)", "NaN"),
        ("SELECT ln('Infinity'::numeric)", "Infinity"),
        ("SELECT ln('NaN'::numeric)", "NaN"),
        ("SELECT log('Infinity'::numeric)", "Infinity"),
        ("SELECT log('NaN'::numeric)", "NaN"),
        ("SELECT exp('Infinity'::numeric)", "Infinity"),
        ("SELECT exp('-Infinity'::numeric)", "0"),
        ("SELECT exp('NaN'::numeric)", "NaN"),
        // scale / min_scale have nothing to report for a special.
        ("SELECT scale('NaN'::numeric)", "NULL"),
        ("SELECT scale('Infinity'::numeric)", "NULL"),
        ("SELECT min_scale('NaN'::numeric)", "NULL"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // The two cells where PG itself raises.
    for (sql, want) in [
        (
            "SELECT sqrt('-Infinity'::numeric)",
            "cannot take square root of a negative number",
        ),
        (
            "SELECT ln('-Infinity'::numeric)",
            "cannot take logarithm of a negative number",
        ),
        (
            "SELECT log('-Infinity'::numeric)",
            "cannot take logarithm of a negative number",
        ),
        (
            "SELECT width_bucket('NaN'::numeric, 0, 10, 5)",
            "operand, lower bound, and upper bound cannot be NaN",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
}

#[test]
fn specials_flow_through_the_binary_family() {
    let mut e = Engine::new();
    for (sql, want) in [
        // div truncates toward zero: infinite dividend stays infinite,
        // infinite divisor gives 0, NaN anywhere gives NaN.
        ("SELECT div('Infinity'::numeric, 2)", "Infinity"),
        ("SELECT div('NaN'::numeric, 2)", "NaN"),
        ("SELECT div(2, 'Infinity'::numeric)", "0"),
        // mod is NaN whenever either side is special — even for an
        // infinite dividend (probed; NOT Infinity).
        ("SELECT mod('NaN'::numeric, 2)", "NaN"),
        ("SELECT mod('Infinity'::numeric, 2)", "NaN"),
        ("SELECT mod(2, 'NaN'::numeric)", "NaN"),
        // log(base, x).
        ("SELECT log(2, 'Infinity'::numeric)", "Infinity"),
        ("SELECT log('Infinity'::numeric, 2)", "0"),
        // power already honoured the specials (v7.38); regression guard.
        ("SELECT power('Infinity'::numeric, 0)", "1"),
        ("SELECT power(2, 'Infinity'::numeric)", "Infinity"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn specials_cross_casts_in_both_directions() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 'Infinity'::numeric::float8", "Infinity"),
        ("SELECT '-Infinity'::numeric::float8", "-Infinity"),
        ("SELECT 'NaN'::numeric::float8", "NaN"),
        ("SELECT 'NaN'::numeric::float4", "NaN"),
        // The reverse direction used to be an outright refusal.
        ("SELECT 'Infinity'::float8::numeric", "Infinity"),
        ("SELECT 'NaN'::float8::numeric", "NaN"),
        ("SELECT '-Infinity'::real::numeric", "-Infinity"),
        // A typmod'd numeric still takes NaN (it has no magnitude).
        ("SELECT 'NaN'::numeric::numeric(10,2)", "NaN"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // The integer targets refuse; PG names an infinity without its sign.
    for (sql, want) in [
        (
            "SELECT 'NaN'::numeric::int",
            "cannot convert NaN to integer",
        ),
        (
            "SELECT 'Infinity'::numeric::int",
            "cannot convert infinity to integer",
        ),
        (
            "SELECT '-Infinity'::numeric::int",
            "cannot convert infinity to integer",
        ),
        (
            "SELECT 'NaN'::numeric::bigint",
            "cannot convert NaN to bigint",
        ),
        (
            "SELECT '-Infinity'::numeric::bigint",
            "cannot convert infinity to bigint",
        ),
        // …and a declared precision overflows on an infinity.
        (
            "SELECT 'Infinity'::numeric::numeric(10,2)",
            "numeric field overflow",
        ),
        (
            "SELECT 'Infinity'::float8::numeric(10,2)",
            "numeric field overflow",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
}

#[test]
fn zero_to_a_negative_power_raises_across_the_tower() {
    let mut e = Engine::new();
    for sql in [
        "SELECT power(0, -1)",
        "SELECT power(0::bigint, -1)",
        "SELECT power(0::smallint, -1)",
        "SELECT power(0::float8, -1::float8)",
        "SELECT power(0::numeric, -1)",
        "SELECT 0 ^ -1",
        "SELECT 0.0 ^ (-1)",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("zero raised to a negative power is undefined"),
            "{sql} → {got}"
        );
    }
    // A zero base with a non-negative exponent is still fine.
    assert_eq!(one(&mut e, "SELECT power(0, 2)"), "0");
    assert_eq!(one(&mut e, "SELECT 2 ^ 3"), "8");
}

#[test]
fn div_is_numeric_and_the_error_wordings_are_pgs() {
    let mut e = Engine::new();
    // PG declares only div(numeric, numeric) — same digits, numeric type.
    assert_eq!(one(&mut e, "SELECT div(9, 4)"), "2");
    assert_eq!(one(&mut e, "SELECT div(-9, 4)"), "-2");
    assert_eq!(one(&mut e, "SELECT pg_typeof(div(9,4))"), "numeric");
    assert_eq!(one(&mut e, "SELECT pg_typeof(div(9::bigint,4))"), "numeric");
    // mod stays integer (PG has the integer overloads).
    assert_eq!(one(&mut e, "SELECT pg_typeof(mod(9,4))"), "integer");
    // Bare "division by zero" — the div(): / mod(): prefixes were leaks.
    for sql in [
        "SELECT div(1, 0)",
        "SELECT mod(1, 0)",
        "SELECT 1/0",
        "SELECT 1.0/0",
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains("division by zero"), "{sql} → {got}");
        assert!(
            !got.contains("div():") && !got.contains("mod():"),
            "{sql} → {got}"
        );
    }
    // An unknown function names the call signature, like PG.
    let got = err(&mut e, "SELECT nosuchfn(1)");
    assert!(
        got.contains("function nosuchfn(integer) does not exist"),
        "{got}"
    );
    let got = err(&mut e, "SELECT nosuchfn()");
    assert!(got.contains("function nosuchfn() does not exist"), "{got}");
}

#[test]
fn the_finite_math_core_is_unchanged() {
    let mut e = Engine::new();
    // Regression guard over the sweep's already-matching cells.
    for (sql, want) in [
        ("SELECT round(2.5)", "3"),
        ("SELECT round(-2.5)", "-3"),
        ("SELECT round(12345.6789, -2)", "12300"),
        ("SELECT trunc(-1.9)", "-1"),
        ("SELECT mod(-9, 4)", "-1"),
        ("SELECT mod(9, -4)", "1"),
        ("SELECT gcd(12, 18)", "6"),
        ("SELECT lcm(4, 6)", "12"),
        ("SELECT factorial(5)", "120"),
        ("SELECT width_bucket(5.35, 0.024, 10.06, 5)", "3"),
        ("SELECT scale(8.4100)", "4"),
        ("SELECT min_scale(8.4100)", "2"),
        ("SELECT trim_scale(8.4100)", "8.41"),
        ("SELECT 5 / 2", "2"),
        ("SELECT 5.0 / 2", "2.5000000000000000"),
        ("SELECT 5::float8 / 2", "2.5"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    for (sql, want) in [
        ("SELECT 2147483647::int + 1", "integer out of range"),
        (
            "SELECT 9223372036854775807::bigint + 1",
            "bigint out of range",
        ),
        ("SELECT 1e308::float8 * 10", "value out of range: overflow"),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
}
