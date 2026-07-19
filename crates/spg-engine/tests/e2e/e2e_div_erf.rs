//! v7.37.17 (17.6 siblings) — div(y, x) + PG 17+ erf/erfc.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_float(v: &spg_storage::Value<'_>) -> f64 {
    match v {
        spg_storage::Value::Float(f) => *f,
        other => panic!("expected Float, got {other:?}"),
    }
}

/// v7.39 (round 254) — this pin locked SPG's pre-r254 integer result
/// shape; PG declares only `div(numeric, numeric)` and reports NUMERIC
/// for every argument type (probed live: `pg_typeof(div(9,4))` =
/// numeric). The digits are unchanged — only the type tag moved.
#[test]
fn div_truncated_quotient() {
    let mut e = Engine::new();
    let quotient = |e: &mut Engine, sql: &str| match first(e, sql) {
        spg_storage::Value::Numeric {
            scaled,
            scale: 0,
            kind: spg_storage::NumericKind::Finite,
        } => scaled,
        other => panic!("{sql}: expected a scale-0 numeric, got {other:?}"),
    };
    assert_eq!(quotient(&mut e, "SELECT div(9, 4)"), 2);
    assert_eq!(quotient(&mut e, "SELECT div(-9, 4)"), -2);
    assert_eq!(quotient(&mut e, "SELECT div(100::bigint, 7::bigint)"), 14);
}

#[test]
fn div_by_zero_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT div(1, 0)").is_err());
}

#[test]
fn erf_known_values() {
    let mut e = Engine::new();
    // erf(0) = 0
    assert!((as_float(&first(&mut e, "SELECT erf(0)"))).abs() < 1e-12);
    // erf(1) ≈ 0.8427007929497149
    let v = as_float(&first(&mut e, "SELECT erf(1)"));
    assert!((v - 0.8427007929497149).abs() < 1e-12, "erf(1) = {v}");
    // erfc(x) = 1 - erf(x)
    let a = as_float(&first(&mut e, "SELECT erf(0.5)"));
    let b = as_float(&first(&mut e, "SELECT erfc(0.5)"));
    assert!((a + b - 1.0).abs() < 1e-12);
}

#[test]
fn div_erf_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT div(NULL::int, 1)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT erf(NULL::float)"),
        spg_storage::Value::Null
    ));
}
