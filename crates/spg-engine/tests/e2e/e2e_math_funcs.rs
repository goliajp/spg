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
    let v = first(&mut e, "SELECT log10(100.0)");
    assert!(approx(as_f64(&v), 2.0, 1e-9), "got {}", as_f64(&v));
    let v = first(&mut e, "SELECT log(100.0)");
    assert!(approx(as_f64(&v), 2.0, 1e-9), "got {}", as_f64(&v));
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
    let v = first(&mut e, "SELECT cbrt(27.0)");
    assert!(approx(as_f64(&v), 3.0, 1e-6), "got {}", as_f64(&v));
    // Negative sign preserved.
    let v = first(&mut e, "SELECT cbrt(-27.0)");
    assert!(approx(as_f64(&v), -3.0, 1e-6), "got {}", as_f64(&v));
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
