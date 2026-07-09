//! v7.38 (read01) — float8 text input rejects out-of-range literals like PG's
//! float8in: an overflow to ±∞ or a nonzero underflow to 0 is an error, not a
//! silent Infinity/0. The inf/infinity/nan spellings and in-range values pass.
//! Shared between the `::float`/`::double` and `::float8` cast paths. Every
//! expected value / error is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn out_of_range_float8_literals_error() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT '1e400'::float8").is_err(), "overflow → out of range");
    assert!(e.execute("SELECT '-1e400'::float8").is_err());
    assert!(e.execute("SELECT '1e-400'::float8").is_err(), "nonzero underflow → out of range");
    // The `::float` / `::double` spelling agrees.
    assert!(e.execute("SELECT '1e400'::float").is_err());
    // Unparseable text is still an error.
    assert!(e.execute("SELECT 'abc'::float8").is_err());
}

#[test]
fn in_range_and_special_float8_literals_pass() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ('1e308'::float8)::text"), "1e+308");
    assert_eq!(one(&mut e, "SELECT ('3.14'::float8)::text"), "3.14");
    assert_eq!(one(&mut e, "SELECT ('inf'::float8)::text"), "Infinity");
    assert_eq!(one(&mut e, "SELECT ('-Infinity'::float8)::text"), "-Infinity");
    assert_eq!(one(&mut e, "SELECT ('nan'::float8)::text"), "NaN");
    // Zero and a zero-mantissa exponent are legitimately zero, not underflow.
    assert_eq!(one(&mut e, "SELECT ('0'::float8)::text"), "0");
    assert_eq!(one(&mut e, "SELECT ('0e-5'::float8)::text"), "0");
    assert_eq!(one(&mut e, "SELECT ('1e-300'::float8)::text"), "1e-300");
}
