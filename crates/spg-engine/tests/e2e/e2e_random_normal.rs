//! v7.37.17 (17.6 siblings) — PG 16+ random_normal.

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
fn random_normal_defaults() {
    let mut e = Engine::new();
    // Default mean=0, stddev=1. Take 100 samples; the empirical mean
    // should be near 0 and within a wide sanity range.
    let mut sum = 0.0f64;
    let mut count = 0;
    for _ in 0..100 {
        let v = as_float(&first(&mut e, "SELECT random_normal()"));
        assert!(v.is_finite(), "random_normal produced non-finite: {v}");
        sum += v;
        count += 1;
    }
    let mean = sum / (count as f64);
    // 100 samples of standard normal have SE ~ 0.1, use 3× that
    // sanity band (0.3) around the true mean.
    assert!(mean.abs() < 0.5, "empirical mean = {mean}, expected near 0");
}

#[test]
fn random_normal_with_mean_stddev() {
    let mut e = Engine::new();
    // mean=100, stddev=5. Verify samples cluster around 100.
    for _ in 0..20 {
        let v = as_float(&first(&mut e, "SELECT random_normal(100, 5)"));
        // 4-sigma band: [80, 120]. Very safe.
        assert!(
            v > 80.0 && v < 120.0,
            "random_normal(100, 5) = {v} out of 4-sigma band"
        );
    }
}

#[test]
fn random_normal_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT random_normal(NULL::float, 1)"),
        spg_storage::Value::Null
    ));
}
