//! v7.37.17 (17.6 siblings) — MySQL to_base64 / from_base64 +
//! sha / sha2 + random_bytes + load_file.

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
fn base64_roundtrip() {
    let mut e = Engine::new();
    // MySQL doc vector: TO_BASE64('abc') → 'YWJj'.
    assert_eq!(text(&first(&mut e, "SELECT to_base64('abc')")), "YWJj");
    // Round-trip through from_base64 (returns binary).
    assert!(matches!(
        first(&mut e, "SELECT from_base64(to_base64('abc'))"),
        spg_storage::Value::Bytes(ref b) if b.as_ref() == b"abc"
    ));
    // Whitespace tolerance on decode.
    assert!(matches!(
        first(&mut e, "SELECT from_base64(concat('YW', chr(10), 'Jj'))"),
        spg_storage::Value::Bytes(ref b) if b.as_ref() == b"abc"
    ));
    // Invalid input → NULL, not an error.
    assert!(matches!(
        first(&mut e, "SELECT from_base64('!!not base64!!')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn sha_and_sha2_vectors() {
    let mut e = Engine::new();
    // Known SHA-1('abc') vector (FIPS 180-1).
    assert_eq!(
        text(&first(&mut e, "SELECT sha('abc')")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    // Known SHA-256('abc') vector; bits=0 defaults to 256.
    let sha256_abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(text(&first(&mut e, "SELECT sha2('abc', 256)")), sha256_abc);
    assert_eq!(text(&first(&mut e, "SELECT sha2('abc', 0)")), sha256_abc);
    // SHA-512 length check.
    assert_eq!(text(&first(&mut e, "SELECT sha2('abc', 512)")).len(), 128);
    // Unsupported bit length → NULL (MySQL).
    assert!(matches!(
        first(&mut e, "SELECT sha2('abc', 100)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn random_bytes_and_load_file() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT random_bytes(16)"),
        spg_storage::Value::Bytes(ref b) if b.len() == 16
    ));
    // NULL without FILE privilege — the unprivileged-client shape.
    assert!(matches!(
        first(&mut e, "SELECT load_file('/etc/passwd')"),
        spg_storage::Value::Null
    ));
}
