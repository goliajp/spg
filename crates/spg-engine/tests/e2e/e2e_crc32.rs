//! v7.37.17 (17.6 siblings) — crc32 + crc32c.

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

fn as_bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn crc32_known_vectors() {
    let mut e = Engine::new();
    // Empty input → 0
    assert_eq!(as_bigint(&first(&mut e, "SELECT crc32('')")), 0);
    // 'a' → 0xE8B7BE43 = 3904355907
    assert_eq!(as_bigint(&first(&mut e, "SELECT crc32('a')")), 0xE8B7_BE43);
    // '123456789' → 0xCBF43926 = 3421780262 (classic vector)
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT crc32('123456789')")),
        0xCBF4_3926
    );
}

#[test]
fn crc32c_known_vectors() {
    let mut e = Engine::new();
    // Empty → 0
    assert_eq!(as_bigint(&first(&mut e, "SELECT crc32c('')")), 0);
    // '123456789' → 0xE3069283 (Castagnoli standard vector)
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT crc32c('123456789')")),
        0xE306_9283
    );
}

#[test]
fn crc32_stable_across_calls() {
    let mut e = Engine::new();
    let a = as_bigint(&first(&mut e, "SELECT crc32('hello')"));
    let b = as_bigint(&first(&mut e, "SELECT crc32('hello')"));
    assert_eq!(a, b);
}

#[test]
fn crc32_bytea_input() {
    let mut e = Engine::new();
    assert_eq!(
        as_bigint(&first(&mut e, "SELECT crc32('a'::bytea)")),
        0xE8B7_BE43
    );
}

#[test]
fn crc32_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT crc32(NULL::text)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT crc32c(NULL::text)"),
        spg_storage::Value::Null
    ));
}
