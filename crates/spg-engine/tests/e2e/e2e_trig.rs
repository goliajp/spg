//! v7.37.17 (17.6 siblings) — trig family sin/cos/tan/asin/acos/
//! atan/atan2 + hyperbolic sinh/cosh/tanh + degree-input variants.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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

fn assert_close(actual: f64, expected: f64, tol: f64) {
    assert!(
        (actual - expected).abs() < tol,
        "expected ~{expected}, got {actual}"
    );
}

#[test]
fn sin_cos_tan_basic_values() {
    let mut e = Engine::new();
    assert_close(as_float(&first(&mut e, "SELECT sin(0)")), 0.0, 1e-12);
    assert_close(as_float(&first(&mut e, "SELECT cos(0)")), 1.0, 1e-12);
    assert_close(as_float(&first(&mut e, "SELECT tan(0)")), 0.0, 1e-12);
    // sin(π/2) = 1
    assert_close(
        as_float(&first(&mut e, "SELECT sin(pi() / 2)")),
        1.0,
        1e-12,
    );
    // cos(π) = -1
    assert_close(as_float(&first(&mut e, "SELECT cos(pi())")), -1.0, 1e-12);
}

#[test]
fn inverse_trig() {
    let mut e = Engine::new();
    // asin(0) = 0, asin(1) = π/2
    assert_close(as_float(&first(&mut e, "SELECT asin(0)")), 0.0, 1e-12);
    assert_close(
        as_float(&first(&mut e, "SELECT asin(1)")),
        core::f64::consts::PI / 2.0,
        1e-12,
    );
    assert_close(as_float(&first(&mut e, "SELECT acos(1)")), 0.0, 1e-12);
    assert_close(as_float(&first(&mut e, "SELECT atan(0)")), 0.0, 1e-12);
    // atan(1) = π/4
    assert_close(
        as_float(&first(&mut e, "SELECT atan(1)")),
        core::f64::consts::PI / 4.0,
        1e-12,
    );
}

#[test]
fn atan2_quadrants() {
    let mut e = Engine::new();
    // atan2(0, 1) = 0
    assert_close(as_float(&first(&mut e, "SELECT atan2(0, 1)")), 0.0, 1e-12);
    // atan2(1, 0) = π/2
    assert_close(
        as_float(&first(&mut e, "SELECT atan2(1, 0)")),
        core::f64::consts::PI / 2.0,
        1e-12,
    );
    // atan2(1, 1) = π/4
    assert_close(
        as_float(&first(&mut e, "SELECT atan2(1, 1)")),
        core::f64::consts::PI / 4.0,
        1e-12,
    );
}

#[test]
fn hyperbolic() {
    let mut e = Engine::new();
    assert_close(as_float(&first(&mut e, "SELECT sinh(0)")), 0.0, 1e-12);
    assert_close(as_float(&first(&mut e, "SELECT cosh(0)")), 1.0, 1e-12);
    assert_close(as_float(&first(&mut e, "SELECT tanh(0)")), 0.0, 1e-12);
    // asinh(sinh(1)) = 1
    let one = as_float(&first(&mut e, "SELECT asinh(sinh(1))"));
    assert_close(one, 1.0, 1e-12);
}

#[test]
fn degree_variants() {
    let mut e = Engine::new();
    // sind(90) = 1
    assert_close(as_float(&first(&mut e, "SELECT sind(90)")), 1.0, 1e-12);
    // cosd(180) = -1
    assert_close(as_float(&first(&mut e, "SELECT cosd(180)")), -1.0, 1e-12);
    // atand(1) = 45
    assert_close(as_float(&first(&mut e, "SELECT atand(1)")), 45.0, 1e-12);
    // atan2d(1, 1) = 45
    assert_close(as_float(&first(&mut e, "SELECT atan2d(1, 1)")), 45.0, 1e-12);
}

#[test]
fn trig_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "sin(NULL::float)",
        "cos(NULL::float)",
        "atan2(NULL::float, 1)",
        "atan2(1, NULL::float)",
        "sind(NULL::float)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
