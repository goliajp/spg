//! v7.37.17 (17.6 siblings) — bytea_xor + bytea_and + bytea_or.

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

fn bytes(v: &spg_storage::Value<'_>) -> Vec<u8> {
    match v {
        spg_storage::Value::Bytes(b) => b.to_vec(),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn bytea_xor_same_length() {
    let mut e = Engine::new();
    // 'AB' = 0x41 0x42, XOR itself = 0x00 0x00
    let v = bytes(&first(&mut e, "SELECT bytea_xor('AB'::bytea, 'AB'::bytea)"));
    assert_eq!(v, [0x00, 0x00]);
    // 'A' 0x41 XOR 0x01 = 0x40
    let v = bytes(&first(
        &mut e,
        "SELECT bytea_xor('A'::bytea, '\\x01'::bytea)",
    ));
    assert_eq!(v, [0x40]);
}

#[test]
fn bytea_xor_length_mismatch_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT bytea_xor('AB'::bytea, 'ABC'::bytea)")
            .is_err()
    );
}

#[test]
fn bytea_and_or() {
    let mut e = Engine::new();
    // 0xF0 AND 0x0F = 0x00
    let v = bytes(&first(
        &mut e,
        "SELECT bytea_and('\\xF0'::bytea, '\\x0F'::bytea)",
    ));
    assert_eq!(v, [0x00]);
    // 0xF0 OR 0x0F = 0xFF
    let v = bytes(&first(
        &mut e,
        "SELECT bytea_or('\\xF0'::bytea, '\\x0F'::bytea)",
    ));
    assert_eq!(v, [0xFF]);
    // 0x0A AND 0x0F = 0x0A; 0x0A OR 0xF0 = 0xFA
    let v = bytes(&first(
        &mut e,
        "SELECT bytea_and('\\x0A'::bytea, '\\x0F'::bytea)",
    ));
    assert_eq!(v, [0x0A]);
    let v = bytes(&first(
        &mut e,
        "SELECT bytea_or('\\x0A'::bytea, '\\xF0'::bytea)",
    ));
    assert_eq!(v, [0xFA]);
}

#[test]
fn bytea_bitops_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT bytea_xor(NULL::bytea, 'A'::bytea)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT bytea_and('A'::bytea, NULL::bytea)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT bytea_or(NULL::bytea, 'A'::bytea)"),
        spg_storage::Value::Null
    ));
}
