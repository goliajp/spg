//! v7.39 (read01 utils/adt, round 19) — numeric.c knives: the POSIX
//! pow(3) special-value table for NUMERIC power, PG's log/sqrt/power
//! domain-error wordings, the NUMERIC gcd()/lcm() overload, the numeric
//! input-syntax wording, and generate_series(numeric). Byte-locked vs
//! PG18.

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

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn power_special_value_table() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT power(0::numeric, 0.5), power('NaN'::numeric, 0), \
             power('NaN'::numeric, 2), power(1::numeric, 'NaN')"
        ),
        vec!["0.0000000000000000", "1", "NaN", "1"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT power('inf'::numeric, 2), power('inf'::numeric, -1), \
             power('-inf'::numeric, 2), power('-inf'::numeric, 3), \
             power('-inf'::numeric, -2)"
        ),
        vec!["Infinity", "0", "Infinity", "-Infinity", "0"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT power(0.5::numeric, 'inf'::numeric), \
             power(0.5::numeric, '-inf'::numeric), \
             power(2::numeric, 'inf'::numeric), \
             power(-1::numeric, 'inf'::numeric), \
             power(0::numeric, 'inf'::numeric)"
        ),
        vec!["0", "Infinity", "Infinity", "1", "0"]
    );
}

#[test]
fn power_log_sqrt_domain_errors() {
    let mut e = Engine::new();
    assert!(
        err_of(&mut e, "SELECT power(0::numeric, '-inf'::numeric)")
            .contains("zero raised to a negative power is undefined")
    );
    assert!(
        err_of(&mut e, "SELECT 0::numeric ^ (-0.5)")
            .contains("zero raised to a negative power is undefined")
    );
    assert!(
        err_of(&mut e, "SELECT (-8)::numeric ^ (1::numeric/3)")
            .contains("a negative number raised to a non-integer power yields a complex result")
    );
    assert!(err_of(&mut e, "SELECT ln(0::numeric)").contains("cannot take logarithm of zero"));
    assert!(
        err_of(&mut e, "SELECT ln(-1::numeric)")
            .contains("cannot take logarithm of a negative number")
    );
    assert!(err_of(&mut e, "SELECT log(0::numeric)").contains("cannot take logarithm of zero"));
    assert!(
        err_of(&mut e, "SELECT sqrt(-1::numeric)")
            .contains("cannot take square root of a negative number")
    );
    assert!(
        err_of(&mut e, "SELECT '-NaN'::numeric")
            .contains("invalid input syntax for type numeric: \"-NaN\"")
    );
}

#[test]
fn numeric_gcd_lcm_overload() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT gcd(12.0::numeric, 8.0::numeric), lcm(4.0::numeric, 6.0::numeric), \
             gcd(1.5, 2.25), gcd('NaN'::numeric, 5), lcm(0.0::numeric, 5.5)"
        ),
        vec!["4.0", "12.0", "0.75", "NaN", "0.0"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT gcd(12, 8.0), lcm(330.3, 462), gcd(0::numeric, 0::numeric)"
        ),
        vec!["4.0", "508662.0", "0"]
    );
}

#[test]
fn generate_series_numeric() {
    let mut e = Engine::new();
    assert_eq!(
        col_of(&mut e, "SELECT generate_series(1.0, 2.0, 0.5)"),
        vec!["1.0", "1.5", "2.0"]
    );
    assert_eq!(
        col_of(&mut e, "SELECT generate_series(1.1, 4)"),
        vec!["1.1", "2.1", "3.1"]
    );
    assert_eq!(
        col_of(&mut e, "SELECT generate_series(4, 3, -1.1)"),
        vec!["4"]
    );
    assert!(
        err_of(&mut e, "SELECT generate_series('NaN'::numeric, 4)")
            .contains("start value cannot be NaN")
    );
    assert!(
        err_of(&mut e, "SELECT generate_series(1, 'inf'::numeric)")
            .contains("stop value cannot be infinity")
    );
    assert!(
        err_of(&mut e, "SELECT generate_series(1::numeric, 4, 0.0)")
            .contains("step size cannot equal zero")
    );
    assert!(
        err_of(&mut e, "SELECT generate_series(1.0, 2.0, 'NaN'::numeric)")
            .contains("step size cannot be NaN")
    );
}
