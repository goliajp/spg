//! v7.37.17 (17.6 siblings) — cot(x) + log2(x).

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

#[test]
fn cot_known_values() {
    let mut e = Engine::new();
    // cot(π/4) = 1
    let v = as_float(&first(&mut e, "SELECT cot(pi() / 4)"));
    assert!((v - 1.0).abs() < 1e-12, "cot(π/4) = {v}");
    // cot(π/2) = 0
    let v = as_float(&first(&mut e, "SELECT cot(pi() / 2)"));
    assert!(v.abs() < 1e-12, "cot(π/2) = {v}");
}

#[test]
fn log2_known_values() {
    let mut e = Engine::new();
    assert!((as_float(&first(&mut e, "SELECT log2(8)")) - 3.0).abs() < 1e-12);
    assert!((as_float(&first(&mut e, "SELECT log2(1024)")) - 10.0).abs() < 1e-12);
    assert!(as_float(&first(&mut e, "SELECT log2(1)")).abs() < 1e-12);
}

#[test]
fn log2_nonpositive_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT log2(0)").is_err());
    assert!(e.execute("SELECT log2(-1)").is_err());
}

#[test]
fn cot_log2_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT cot(NULL::float)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT log2(NULL::float)"),
        spg_storage::Value::Null
    ));
}
