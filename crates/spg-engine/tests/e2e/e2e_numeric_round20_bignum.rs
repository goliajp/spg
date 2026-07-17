//! v7.39 (read01 utils/adt, round 20) — numeric.c part 2: scientific
//! literals are NUMERIC (expanded exactly, no float round-trip), the
//! bignum literal/cast pipeline past i128, and the sum/avg bignum spill
//! (no more i128 saturation). Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn scientific_literals_are_numeric() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT 1e5, 1.5e3, 1e-5, 1.5e-3, 2E+2, 1e0"),
        vec!["100000", "1500", "0.00001", "0.0015", "200", "1"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT 1.23456e2, 1.2e-2, 1.500e1, 0.5e1"),
        vec!["123.456", "0.012", "15.00", "5"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT pg_typeof(1e5), pg_typeof(1.5e3), pg_typeof(1e-5), pg_typeof(1e300)"
        ),
        vec!["numeric", "numeric", "numeric", "numeric"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT 1e5::int, 1e5 + 1"),
        vec!["100000", "100001"]
    );
}

#[test]
fn big_literals_expand_exactly() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT length(1e300::text), left(1e300::text, 5), 1e300 = 1e300"
        ),
        vec!["301", "10000", "true"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT length(1e100000::text)"),
        vec!["100001"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT 1e30, -2.5e30"),
        vec![
            "1000000000000000000000000000000",
            "-2500000000000000000000000000000"
        ]
    );
    // Text-cast form accepts the exponent too.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '1e300'::numeric = 1e300, length('1.5e100'::numeric::text)"
        ),
        vec!["true", "101"]
    );
    // Mixed big-numeric arithmetic and the float8 crossover.
    assert_eq!(
        row_of(&mut e, "SELECT 1e20 * 1e20, (3.14e100)::float8::text"),
        vec!["10000000000000000000000000000000000000000", "3.14e+100"]
    );
    // Beyond PG's numeric format: dedicated error.
    assert!(err_of(&mut e, "SELECT 1e131072").contains("value overflows numeric format"));
    assert!(
        err_of(&mut e, "SELECT '1e131072'::numeric").contains("value overflows numeric format")
    );
}

#[test]
fn sum_avg_spill_past_i128() {
    let mut e = Engine::new();
    // Two 38-digit values: the exact sum leaves i128 — PG renders all
    // digits (SPG previously saturated at i128::MAX).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT sum(x) FROM (VALUES (90000000000000000000000000000000000000::numeric),\
             (90000000000000000000000000000000000000)) t(x)"
        ),
        vec!["180000000000000000000000000000000000000"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT sum(x) FROM (VALUES (90000000000000000000000000000000000000.25::numeric),\
             (90000000000000000000000000000000000000)) t(x)"
        ),
        vec!["180000000000000000000000000000000000000.25"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT avg(x) FROM (VALUES (90000000000000000000000000000000000000::numeric),\
             (90000000000000000000000000000000000000)) t(x)"
        ),
        vec!["90000000000000000000000000000000000000"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT avg(x) FROM (VALUES (90000000000000000000000000000000000000.5::numeric),\
             (90000000000000000000000000000000000000)) t(x)"
        ),
        vec!["90000000000000000000000000000000000000.3"]
    );
    // In-range sums keep the exact i128 lane (all-digit rendering).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT sum(x), avg(x) FROM (VALUES (1e30::numeric),(1.5e30)) t(x)"
        ),
        vec![
            "2500000000000000000000000000000",
            "1250000000000000000000000000000"
        ]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT avg(x) FROM (VALUES (1e37::numeric),(2e37),(2e37)) t(x)"
        ),
        vec!["16666666666666666666666666666666666667"]
    );
}
