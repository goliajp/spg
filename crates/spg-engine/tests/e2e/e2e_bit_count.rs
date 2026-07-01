//! v7.37.17 (17.6 siblings) — PG 14+ bit_count(x) popcount.

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
fn bit_count_int() {
    let mut e = Engine::new();
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(0)")), 0);
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(1)")), 1);
    // 7 = 0b111
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(7)")), 3);
    // 255 = 0b11111111
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(255)")), 8);
    // 65535 = 0b1111111111111111
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(65535)")), 16);
}

#[test]
fn bit_count_bigint() {
    let mut e = Engine::new();
    // 2^63 - 1 = i64::MAX = 63 ones.
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT bit_count(9223372036854775807::bigint)")),
        63
    );
}

#[test]
fn bit_count_bytea() {
    let mut e = Engine::new();
    // 'A' = 0x41 = 0b01000001 → 2 bits.
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count('A'::bytea)")), 2);
    // 'AB' = 0x41 0x42 = 2 + 2 = 4 bits.
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count('AB'::bytea)")), 4);
    // Empty bytea → 0.
    assert_eq!(as_bigint(&first(&mut e, "SELECT bit_count(''::bytea)")), 0);
}

#[test]
fn bit_count_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT bit_count(NULL::int)"),
        spg_storage::Value::Null
    ));
}
