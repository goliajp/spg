//! v7.37.17 (17.6 siblings) — MySQL-compat conversion functions:
//! hex / unhex / conv / bin / oct / ord + mid / lcase / ucase
//! aliases.

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn hex_int_and_text() {
    let mut e = Engine::new();
    // MySQL doc vectors: HEX(255) = 'FF', HEX('abc') = '616263'.
    assert_eq!(text(&first(&mut e, "SELECT hex(255)")), "FF");
    assert_eq!(text(&first(&mut e, "SELECT hex('abc')")), "616263");
}

#[test]
fn unhex_roundtrip_and_invalid() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT unhex('616263')") {
        spg_storage::Value::Bytes(b) => assert_eq!(b.as_ref(), b"abc"),
        other => panic!("got {other:?}"),
    }
    // Invalid hex → NULL (MySQL semantics, not an error).
    assert!(matches!(
        first(&mut e, "SELECT unhex('zz')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT unhex('abc')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn conv_base_conversion() {
    let mut e = Engine::new();
    // MySQL doc vector: CONV('a', 16, 2) = '1010'.
    assert_eq!(text(&first(&mut e, "SELECT conv('a', 16, 2)")), "1010");
    assert_eq!(text(&first(&mut e, "SELECT conv(255, 10, 16)")), "FF");
    assert_eq!(text(&first(&mut e, "SELECT conv('ff', 16, 10)")), "255");
    // Out-of-range base → NULL.
    assert!(matches!(
        first(&mut e, "SELECT conv('1', 1, 10)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn bin_oct_ord() {
    let mut e = Engine::new();
    // MySQL doc vectors: BIN(12) = '1100', OCT(12) = '14'.
    assert_eq!(text(&first(&mut e, "SELECT bin(12)")), "1100");
    assert_eq!(text(&first(&mut e, "SELECT oct(12)")), "14");
    // ORD('2') = 50 (ASCII); multi-byte reads UTF-8 bytes BE.
    assert!(matches!(
        first(&mut e, "SELECT ord('2')"),
        spg_storage::Value::BigInt(50)
    ));
    assert!(matches!(
        first(&mut e, "SELECT ord('')"),
        spg_storage::Value::BigInt(0)
    ));
}

#[test]
fn mid_lcase_ucase_aliases() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT mid('foobar', 2, 3)")), "oob");
    assert_eq!(text(&first(&mut e, "SELECT lcase('ABC')")), "abc");
    assert_eq!(text(&first(&mut e, "SELECT ucase('abc')")), "ABC");
}

#[test]
fn conv_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "hex(NULL::int)",
        "unhex(NULL::text)",
        "conv(NULL::text, 16, 2)",
        "bin(NULL::int)",
        "ord(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
