//! v7.37.17 (17.6 siblings) — PG 9.6+ num_nulls / num_nonnulls.

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

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn num_nulls_counts() {
    let mut e = Engine::new();
    assert_eq!(
        as_int(&first(&mut e, "SELECT num_nulls(1, NULL, 'a', NULL)")),
        2
    );
    assert_eq!(as_int(&first(&mut e, "SELECT num_nulls(1, 2, 3)")), 0);
    assert_eq!(as_int(&first(&mut e, "SELECT num_nulls(NULL, NULL)")), 2);
}

#[test]
fn num_nonnulls_counts() {
    let mut e = Engine::new();
    assert_eq!(
        as_int(&first(&mut e, "SELECT num_nonnulls(1, NULL, 'a', NULL)")),
        2
    );
    assert_eq!(as_int(&first(&mut e, "SELECT num_nonnulls(1, 2, 3)")), 3);
    assert_eq!(as_int(&first(&mut e, "SELECT num_nonnulls(NULL)")), 0);
}

#[test]
fn num_nulls_zero_args() {
    let mut e = Engine::new();
    assert_eq!(as_int(&first(&mut e, "SELECT num_nulls()")), 0);
    assert_eq!(as_int(&first(&mut e, "SELECT num_nonnulls()")), 0);
}
