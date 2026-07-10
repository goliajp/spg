//! v7.37.17 (17.6 siblings) — convert_from + convert_to text/bytea
//! roundtrip.

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

#[test]
fn convert_from_utf8_bytes_to_text() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT convert_from('hello'::bytea, 'UTF8')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "hello"),
        other => panic!("got {other:?}"),
    }
    // Multi-byte UTF-8: 中 = E4 B8 AD.
    match first(&mut e, "SELECT convert_from('中'::bytea, 'UTF8')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "中"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_to_utf8_text_to_bytes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT convert_to('hello', 'UTF8')") {
        spg_storage::Value::Bytes(b) => assert_eq!(b.as_ref(), b"hello"),
        other => panic!("got {other:?}"),
    }
    // Multi-byte UTF-8 round-trip.
    match first(&mut e, "SELECT convert_to('中', 'UTF8')") {
        spg_storage::Value::Bytes(b) => assert_eq!(b.as_ref(), &[0xE4u8, 0xB8, 0xAD][..]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_round_trip() {
    let mut e = Engine::new();
    // convert_from(convert_to('hello world', 'UTF8'), 'UTF8') → 'hello world'
    match first(
        &mut e,
        "SELECT convert_from(convert_to('hello world', 'UTF8'), 'UTF8')",
    ) {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "hello world"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_unsupported_encoding_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT convert_from('x'::bytea, 'EUC_JP')")
            .is_err()
    );
    assert!(e.execute("SELECT convert_to('x', 'BIG5')").is_err());
}

#[test]
fn convert_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT convert_from(NULL::bytea, 'UTF8')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT convert_to(NULL::text, 'UTF8')"),
        spg_storage::Value::Null
    ));
}

// read01 A-group U33 — LATIN1 (ISO-8859-1) really transcodes now: each
// byte maps 1:1 to U+0000..=U+00FF. Previously it was whitelisted but
// reinterpreted as UTF-8 (silent-wrong: 0xE9 errored / a UTF-8 'é'
// round-tripped to two bytes). Values asserted against live PG 18.4.

#[test]
fn convert_from_latin1_high_byte() {
    let mut e = Engine::new();
    // PG18.4: convert_from('\xe9', 'LATIN1') = 'é'; '\xff' = 'ÿ'.
    match first(&mut e, "SELECT convert_from('\\xe9'::bytea, 'LATIN1')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "é"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT convert_from('\\xff'::bytea, 'LATIN1')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "ÿ"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_to_latin1_high_byte() {
    let mut e = Engine::new();
    // PG18.4: convert_to('é', 'LATIN1') = 0xe9; 'café' = 63 61 66 e9.
    match first(&mut e, "SELECT convert_to('é', 'LATIN1')") {
        spg_storage::Value::Bytes(b) => assert_eq!(b.as_ref(), &[0xE9u8][..]),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT convert_to('café', 'LATIN1')") {
        spg_storage::Value::Bytes(b) => {
            assert_eq!(b.as_ref(), &[0x63u8, 0x61, 0x66, 0xE9][..]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_latin1_ascii_round_trip() {
    let mut e = Engine::new();
    match first(
        &mut e,
        "SELECT convert_from(convert_to('Hello', 'LATIN1'), 'LATIN1')",
    ) {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "Hello"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn convert_to_latin1_unrepresentable_errors() {
    let mut e = Engine::new();
    // PG18.4: '€' (U+20AC) has no LATIN1 equivalent → error.
    assert!(e.execute("SELECT convert_to('€', 'LATIN1')").is_err());
}
