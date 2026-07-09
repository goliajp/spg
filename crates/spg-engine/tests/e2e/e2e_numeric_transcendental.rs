//! v7.38 (S1.1b) — ln / exp / fractional & negative power over NUMERIC compute
//! exact arbitrary-precision NUMERIC (PG's ~16-significant-digit display
//! scale), not a lossy f64. BigNumeric range-reduced Taylor (exp) / atanh (ln).
//! Every expected value is byte-for-byte from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn ln_numeric_is_exact() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ln(2.0)::text"), "0.6931471805599453");
    assert_eq!(text(&mut e, "SELECT ln(10.0)::text"), "2.3025850929940457");
    // Display scale shrinks as the integer part grows (17 - int_digits).
    assert_eq!(text(&mut e, "SELECT ln(999999.0)::text"), "13.815509557963774");
    assert_eq!(text(&mut e, "SELECT ln(1.5)::text"), "0.4054651081081644");
    assert_eq!(text(&mut e, "SELECT ln(0.001)::text"), "-6.9077552789821371");
    assert_eq!(text(&mut e, "SELECT pg_typeof(ln(2.0))::text"), "numeric");
    // A non-positive argument errors.
    assert!(e.execute("SELECT ln(0.0)").is_err());
    assert!(e.execute("SELECT ln(-1.0)").is_err());
}

#[test]
fn exp_numeric_is_exact() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT exp(1.0)::text"), "2.7182818284590452");
    assert_eq!(text(&mut e, "SELECT exp(2.0)::text"), "7.3890560989306502");
    assert_eq!(text(&mut e, "SELECT exp(0.5)::text"), "1.6487212707001281");
    assert_eq!(text(&mut e, "SELECT exp(-1.0)::text"), "0.3678794411714423");
    assert_eq!(text(&mut e, "SELECT exp(10.0)::text"), "22026.465794806717");
    assert_eq!(text(&mut e, "SELECT pg_typeof(exp(1.0))::text"), "numeric");
}

#[test]
fn fractional_and_negative_power_is_exact_numeric() {
    // This is the case that motivated the work: 2.0^0.5 used to be typed
    // NUMERIC but carry a Float value. Now it is exact numeric.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (2.0 ^ 0.5)::text"), "1.4142135623730950");
    assert_eq!(text(&mut e, "SELECT (10.0 ^ 0.5)::text"), "3.1622776601683793");
    assert_eq!(text(&mut e, "SELECT (2.0 ^ (-1))::text"), "0.5000000000000000");
    assert_eq!(text(&mut e, "SELECT power(2.0, 0.5)::text"), "1.4142135623730950");
    assert_eq!(text(&mut e, "SELECT pg_typeof(2.0 ^ 0.5)::text"), "numeric");
    // Non-negative integer exponent stays exact (existing path); an integer
    // base is double in PG, so 2^0.5 (int base) is unaffected here.
    assert_eq!(text(&mut e, "SELECT (2.0 ^ 3)::text"), "8.0000000000000000");
    // Domain errors.
    assert!(e.execute("SELECT (-2.0) ^ 0.5").is_err());
    assert!(e.execute("SELECT 0.0 ^ (-1)").is_err());
}

#[test]
fn float_arguments_keep_the_double_overload() {
    // ln/exp of a float8 stay double precision (PG overload resolution).
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT pg_typeof(ln(2.0::float8))::text"), "double precision");
    assert_eq!(text(&mut e, "SELECT pg_typeof(exp(1.0::float8))::text"), "double precision");
}
