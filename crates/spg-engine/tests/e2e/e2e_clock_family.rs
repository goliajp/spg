//! v7.37.17 (17.6 siblings) — PG clock function family:
//! statement_timestamp / transaction_timestamp / clock_timestamp /
//! localtimestamp / localtime.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn statement_timestamp_returns_timestamp() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert!(matches!(
        first(&mut e, "SELECT statement_timestamp()"),
        spg_storage::Value::Timestamp(_)
    ));
}

#[test]
fn transaction_timestamp_returns_timestamp() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert!(matches!(
        first(&mut e, "SELECT transaction_timestamp()"),
        spg_storage::Value::Timestamp(_)
    ));
}

#[test]
fn clock_timestamp_returns_timestamp() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert!(matches!(
        first(&mut e, "SELECT clock_timestamp()"),
        spg_storage::Value::Timestamp(_)
    ));
}

#[test]
fn localtimestamp_bare_and_parens_both_work() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert!(matches!(
        first(&mut e, "SELECT localtimestamp"),
        spg_storage::Value::Timestamp(_)
    ));
    assert!(matches!(
        first(&mut e, "SELECT localtimestamp()"),
        spg_storage::Value::Timestamp(_)
    ));
}

#[test]
fn localtime_returns_timestamp_alias() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert!(matches!(
        first(&mut e, "SELECT localtime()"),
        spg_storage::Value::Timestamp(_)
    ));
}
