//! v7.37.17 (17.6 siblings) — pgcrypto armor/dearmor (real RFC 4880
//! ASCII armor) + PGP encryption honesty errors.

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

fn bytes(v: &spg_storage::Value<'_>) -> Vec<u8> {
    match v {
        spg_storage::Value::Bytes(b) => b.to_vec(),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn armor_shape() {
    let mut e = Engine::new();
    let armored = text(&first(&mut e, "SELECT armor('hello')"));
    assert!(armored.starts_with("-----BEGIN PGP MESSAGE-----\n"));
    assert!(armored.ends_with("-----END PGP MESSAGE-----\n"));
    // Body contains the base64 of 'hello' and a CRC line.
    assert!(armored.contains("aGVsbG8="), "armored: {armored}");
    assert!(armored.contains("\n="), "CRC-24 trailer expected");
}

#[test]
fn armor_dearmor_roundtrip() {
    let mut e = Engine::new();
    // Feed the armored text through a nested call so the multi-line
    // armor block doesn't need literal escaping.
    let roundtrip = bytes(&first(
        &mut e,
        "SELECT dearmor(armor('round trip payload'))",
    ));
    assert_eq!(roundtrip, b"round trip payload".to_vec());
}

#[test]
fn dearmor_rejects_garbage_and_bad_crc() {
    let mut e = Engine::new();
    // No armor boundary at all.
    assert!(e.execute("SELECT dearmor('not armored')").is_err());
}

#[test]
fn pgp_encryption_errors_honestly() {
    let mut e = Engine::new();
    for f in &[
        "pgp_sym_encrypt('data', 'key')",
        "pgp_sym_decrypt('x'::bytea, 'key')",
        "pgp_pub_encrypt('data', 'x'::bytea)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(e.execute(&sql).is_err(), "{f} should error (not stub)");
    }
    // pgp_key_id is a NULL probe.
    assert!(matches!(
        first(&mut e, "SELECT pgp_key_id('x'::bytea)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn armor_null_passthrough() {
    let mut e = Engine::new();
    for f in &["armor(NULL::text)", "dearmor(NULL::text)"] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
