//! v7.37.17 (17.6 siblings) — PG 17+ random_int(min, max).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn random_int_in_range() {
    let mut e = Engine::new();
    for _ in 0..100 {
        let v = as_bigint(&first(&mut e, "SELECT random_int(10, 20)"));
        assert!(v >= 10 && v <= 20, "random_int(10, 20) = {v} out of range");
    }
}

#[test]
fn random_int_single_value() {
    let mut e = Engine::new();
    assert_eq!(as_bigint(&first(&mut e, "SELECT random_int(5, 5)")), 5);
}

#[test]
fn random_int_negative_range() {
    let mut e = Engine::new();
    for _ in 0..50 {
        let v = as_bigint(&first(&mut e, "SELECT random_int(-10, 10)"));
        assert!(v >= -10 && v <= 10, "random_int(-10, 10) = {v} out of range");
    }
}

#[test]
fn random_int_bigint_range() {
    let mut e = Engine::new();
    for _ in 0..20 {
        let v = as_bigint(&first(
            &mut e,
            "SELECT random_int(0::bigint, 1000000000000::bigint)",
        ));
        assert!(v >= 0 && v <= 1_000_000_000_000);
    }
}

#[test]
fn random_int_min_greater_than_max_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT random_int(20, 10)").is_err());
}

#[test]
fn random_int_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT random_int(NULL::int, 10)"),
        spg_storage::Value::Null
    ));
}

// Reproducibility with setseed is verified in e2e_setseed.rs via
// SELECT random(). random_int adds a modulo step on top; state
// synchronization tests across separate SQL statements can pick
// up side effects from query-stat tracking, so we don't gate on
// bit-exact reproduction of random_int here.
