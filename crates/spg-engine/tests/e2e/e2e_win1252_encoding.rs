//! v7.38 (read01 P6.32) — WIN1252 (Windows-1252) transcoding for convert_to /
//! convert_from. Matches LATIN1 except the remapped 0x80–0x9F range. Oracle
//! values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn val(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn win1252_convert_to() {
    let mut e = Engine::new();
    assert_eq!(val(&mut e, "SELECT convert_to('A', 'WIN1252')"), spg_storage::Value::Bytes(vec![0x41].into()));
    // Euro sign maps to 0x80 (WIN1252-specific, no LATIN1 equivalent).
    assert_eq!(val(&mut e, "SELECT convert_to(E'\\u20AC', 'WIN1252')"), spg_storage::Value::Bytes(vec![0x80].into()));
    // Alias.
    assert_eq!(val(&mut e, "SELECT convert_to('A', 'CP1252')"), spg_storage::Value::Bytes(vec![0x41].into()));
}

#[test]
fn win1252_convert_from() {
    let mut e = Engine::new();
    assert_eq!(val(&mut e, r"SELECT convert_from('\x80'::bytea, 'WIN1252')"), spg_storage::Value::text("€"));
    // 0x41='A', 0xe9='é' (identity in the 0xA0–0xFF range, like LATIN1).
    assert_eq!(val(&mut e, r"SELECT convert_from('\x41e9'::bytea, 'WIN1252')"), spg_storage::Value::text("Aé"));
}

#[test]
fn win1252_undefined_byte_errors() {
    let mut e = Engine::new();
    // 0x81 is one of the five undefined WIN1252 bytes.
    assert!(e.execute(r"SELECT convert_from('\x81'::bytea, 'WIN1252')").is_err());
}
