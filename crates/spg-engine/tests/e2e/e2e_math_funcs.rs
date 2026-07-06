//! v7.37.17 (17.6 siblings) — PG math functions: ln/log/log10/exp/
//! cbrt/pi/gcd/lcm/radians/degrees.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_f64(v: &spg_storage::Value<'_>) -> f64 {
    match v {
        spg_storage::Value::Float(f) => *f,
        other => panic!("expected Float, got {other:?}"),
    }
}

fn as_bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

fn approx(a: f64, b: f64, eps: f64) -> bool {
    (a - b).abs() < eps
}

#[test]
fn ln_of_e_is_one() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT ln(exp(1))");
    assert!(approx(as_f64(&v), 1.0, 1e-9), "got {}", as_f64(&v));
}

#[test]
fn log10_of_100_is_2() {
    let mut e = Engine::new();
    // libm::log10 lands exact powers of ten on whole numbers (live
    // PG18.4: log10(1000) = 3, not the 2.9999999999999996 that
    // ln(x)/ln(10) produced). PG's log(x) is base-10 too.
    assert_eq!(as_f64(&first(&mut e, "SELECT log10(100.0)")), 2.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT log(100.0)")), 2.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT log10(1000.0)")), 3.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT log10(0.001)")), -3.0);
    // Non-power argument matches PG's exact double.
    assert_eq!(
        as_f64(&first(&mut e, "SELECT log10(2.0)")),
        0.301_029_995_663_981_2
    );
}

#[test]
fn log_base_2_of_8_is_3() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT log(2.0, 8.0)");
    assert!(approx(as_f64(&v), 3.0, 1e-9), "got {}", as_f64(&v));
}

#[test]
fn cbrt_of_27_is_3() {
    let mut e = Engine::new();
    // libm::cbrt round-trips perfect cubes EXACTLY (live PG18.4:
    // cbrt(27) = 3, not the 3.0000000000000004 the old
    // exp(ln|x|/3) approximation produced).
    assert_eq!(as_f64(&first(&mut e, "SELECT cbrt(27.0)")), 3.0);
    // Negative sign preserved, exact.
    assert_eq!(as_f64(&first(&mut e, "SELECT cbrt(-27.0)")), -3.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT cbrt(1000000.0)")), 100.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT cbrt(-64.0)")), -4.0);
    // Non-perfect cube matches PG18.4's exact double bit-for-bit.
    assert_eq!(
        as_f64(&first(&mut e, "SELECT cbrt(2.0)")),
        1.259_921_049_894_873_2
    );
}

#[test]
fn sqrt_matches_pg_to_the_ulp() {
    let mut e = Engine::new();
    // libm::sqrt is correctly rounded (IEEE-754) — the same square
    // root PG calls via C libm. Perfect squares are exact and
    // non-squares match PG's double bit-for-bit (the old Newton
    // iteration returned 1.414213562373095, a ULP shy of PG's
    // 1.4142135623730951).
    assert_eq!(as_f64(&first(&mut e, "SELECT sqrt(16.0)")), 4.0);
    assert_eq!(as_f64(&first(&mut e, "SELECT sqrt(2.0)")), 1.414_213_562_373_095_1);
    assert_eq!(
        as_f64(&first(&mut e, "SELECT sqrt(130.0)")),
        11.401_754_250_991_38
    );
}

#[test]
fn pi_returns_pi() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT pi()");
    assert!(
        approx(as_f64(&v), core::f64::consts::PI, 1e-15),
        "got {}",
        as_f64(&v)
    );
}

#[test]
fn gcd_lcm_basic() {
    let mut e = Engine::new();
    assert_eq!(as_bigint(&first(&mut e, "SELECT gcd(12, 18)")), 6);
    assert_eq!(as_bigint(&first(&mut e, "SELECT gcd(0, 5)")), 5);
    assert_eq!(as_bigint(&first(&mut e, "SELECT gcd(-12, 18)")), 6);
    assert_eq!(as_bigint(&first(&mut e, "SELECT lcm(4, 6)")), 12);
    assert_eq!(as_bigint(&first(&mut e, "SELECT lcm(0, 5)")), 0);
}

#[test]
fn radians_degrees_roundtrip() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT degrees(radians(90.0))");
    assert!(approx(as_f64(&v), 90.0, 1e-9), "got {}", as_f64(&v));
}

#[test]
fn float8_overflow_and_underflow_error_like_pg() {
    // PG's check_float8_val: a finite operation overflowing to ±Inf, or a
    // multiply/divide underflowing a non-zero result to 0, is an error —
    // not a silent Inf/0. Inf/NaN operands and additive cancellation pass.
    // All live-PG18.4-verified.
    let mut e = Engine::new();
    for sql in [
        "SELECT 1e308::float8 * 10",
        "SELECT 1e308::float8 + 1e308",
        "SELECT (-1e308::float8) - 1e308",
        "SELECT 1e308::float8 / 1e-10",
        "SELECT 1e-300::float8 * 1e-300", // underflow
    ] {
        assert!(e.execute(sql).is_err(), "{sql} should error (overflow/underflow)");
    }
    // Legitimate results are unaffected.
    assert_eq!(as_f64(&first(&mut e, "SELECT 2.0::float8 * 3.0")), 6.0);
    // An Inf operand keeps its Inf result (no overflow error), and
    // additive cancellation to 0 is fine.
    assert!(as_f64(&first(&mut e, "SELECT 'inf'::float8 * 2")).is_infinite());
    assert_eq!(as_f64(&first(&mut e, "SELECT 1e-300::float8 - 1e-300")), 0.0);
    // NaN propagates without erroring.
    assert!(as_f64(&first(&mut e, "SELECT 'nan'::float8 * 2")).is_nan());
}

#[test]
fn float8_out_uses_scientific_notation_like_pg() {
    // PG float8out: shortest round-trip, scientific when the base-10
    // exponent is < -4 or > 14, fixed otherwise. Every value is
    // live-PG18.4-verified (scalar, array literal, and float8[] column).
    let mut e = Engine::new();
    let txt = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                o => format!("{o:?}"),
            },
            o => format!("{o:?}"),
        }
    };
    for (expr, want) in [
        ("1e14", "100000000000000"),   // E=14 → fixed
        ("1e15", "1e+15"),             // E=15 → scientific
        ("1e20", "1e+20"),
        ("0.0001", "0.0001"),          // E=-4 → fixed
        ("0.00001", "1e-05"),          // E=-5 → scientific
        ("123456.789", "123456.789"),
        ("1234567890123456", "1.234567890123456e+15"),
        ("-2.5e-10", "-2.5e-10"),
        ("3.14e100", "3.14e+100"),
        ("1000000.0", "1000000"),
        ("0.0", "0"),
        ("-0.0", "-0"),
    ] {
        assert_eq!(
            txt(&mut e, &format!("SELECT ({expr})::float8::text")),
            want,
            "float8out({expr})"
        );
    }
    // Array literal + column paths use float8out too.
    assert_eq!(
        txt(&mut e, "SELECT (ARRAY[1e30::float8, 2.0, 0.00001])::text"),
        "{1e+30,2,1e-05}"
    );
    // Non-finite values keep their float8out spelling.
    assert_eq!(txt(&mut e, "SELECT ('inf'::float8)::text"), "Infinity");
    assert_eq!(txt(&mut e, "SELECT ('-inf'::float8)::text"), "-Infinity");
    assert_eq!(txt(&mut e, "SELECT ('nan'::float8)::text"), "NaN");
}
