//! v7.37.17 (17.6 siblings) — PG 18 uuidv7 + uuid_extract_version
//! + uuid_extract_timestamp.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn uuid_bytes(v: &spg_storage::Value<'_>) -> [u8; 16] {
    match v {
        spg_storage::Value::Uuid(b) => *b,
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn uuidv7_has_version_7_and_variant() {
    let mut e = Engine::new();
    let b = uuid_bytes(&first(&mut e, "SELECT uuidv7()"));
    // Version nibble = 7.
    assert_eq!(b[6] >> 4, 7, "version nibble should be 7");
    // Variant bits = 10xx.
    assert_eq!(b[8] >> 6, 0b10, "variant bits should be 10");
}

#[test]
fn uuidv7_unique_across_calls() {
    let mut e = Engine::new();
    let a = uuid_bytes(&first(&mut e, "SELECT uuidv7()"));
    let b = uuid_bytes(&first(&mut e, "SELECT uuidv7()"));
    assert_ne!(a, b);
}

#[test]
fn uuid_extract_version_works() {
    let mut e = Engine::new();
    // v7 UUID → 7.
    match first(&mut e, "SELECT uuid_extract_version(uuidv7())") {
        spg_storage::Value::Int(7) => {}
        other => panic!("got {other:?}"),
    }
    // v4 UUID → 4.
    match first(&mut e, "SELECT uuid_extract_version(gen_random_uuid())") {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn uuid_extract_timestamp_v7_returns_timestamp() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT uuid_extract_timestamp(uuidv7())") {
        spg_storage::Value::Timestamp(t) => {
            // Should be ≥ the 2020-01-01 anchor (in micros).
            assert!(t >= 1_577_836_800_000_000, "ts = {t}");
        }
        other => panic!("got {other:?}"),
    }
    // v4 UUID has no timestamp → NULL.
    assert!(matches!(
        first(&mut e, "SELECT uuid_extract_timestamp(gen_random_uuid())"),
        spg_storage::Value::Null
    ));
}

#[test]
fn uuid_fns_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT uuid_extract_version(NULL::uuid)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT uuid_extract_timestamp(NULL::uuid)"),
        spg_storage::Value::Null
    ));
}
