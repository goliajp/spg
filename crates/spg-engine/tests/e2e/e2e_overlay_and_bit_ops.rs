//! v7.37.17 (17.6 siblings) — SQL:2003 OVERLAY + get_byte + get_bit.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn overlay_3_arg_uses_placing_len() {
    let mut e = Engine::new();
    // overlay('Txxxxas' placing 'hom' from 2) → 'Thomxas'
    // (replaces 3 chars starting at pos 2 with 3-char 'hom')
    match first(&mut e, "SELECT overlay('Txxxxas', 'hom', 2)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "Thomxas"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn overlay_4_arg_explicit_len() {
    let mut e = Engine::new();
    // overlay('Txxxxas' placing 'hom' from 2 for 4) → 'Thomas'
    // (replaces 4 chars starting at pos 2 with 'hom')
    match first(&mut e, "SELECT overlay('Txxxxas', 'hom', 2, 4)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "Thomas"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn overlay_null_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT overlay(NULL::text, 'x', 2)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn get_byte_returns_byte_value() {
    let mut e = Engine::new();
    // 'a' = 97, 'b' = 98, 'c' = 99
    match first(&mut e, "SELECT get_byte('abc'::bytea, 0)") {
        spg_storage::Value::Int(97) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT get_byte('abc'::bytea, 2)") {
        spg_storage::Value::Int(99) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn set_byte_returns_modified_bytea() {
    let mut e = Engine::new();
    // set_byte('abc', 0, 122) → 'zbc' (122 = 'z')
    match first(&mut e, "SELECT set_byte('abc'::bytea, 0, 122)") {
        spg_storage::Value::Bytes(b) => {
            assert_eq!(b.as_ref(), &[122u8, 98, 99][..]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn set_bit_roundtrips_with_get_bit() {
    let mut e = Engine::new();
    // set_bit('A' (0x41 = 0b01000001), 7, 1) sets the MSB → 0xC1 = 193
    match first(&mut e, "SELECT get_byte(set_bit('A'::bytea, 7, 1), 0)") {
        spg_storage::Value::Int(0xC1) => {}
        other => panic!("got {other:?}"),
    }
    // set_bit('A', 0, 0) clears the LSB → 0x40 = 64 = '@'
    match first(&mut e, "SELECT get_byte(set_bit('A'::bytea, 0, 0), 0)") {
        spg_storage::Value::Int(0x40) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn get_bit_returns_bit() {
    let mut e = Engine::new();
    // 'A' = 0x41 = 0b01000001
    // bit 0 (LSB) = 1
    // bit 6 = 1
    // bit 7 = 0
    match first(&mut e, "SELECT get_bit('A'::bytea, 0)") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT get_bit('A'::bytea, 6)") {
        spg_storage::Value::Int(1) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT get_bit('A'::bytea, 7)") {
        spg_storage::Value::Int(0) => {}
        other => panic!("got {other:?}"),
    }
}
