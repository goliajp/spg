//! v7.37.17 (17.6 siblings) — pgcrypto hmac(data, key, type).

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

fn to_hex(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Bytes(b) => b.iter().map(|byte| format!("{byte:02x}")).collect(),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn hmac_sha256_known_vector() {
    let mut e = Engine::new();
    // RFC 4231 test vector 1:
    //   key = 0x0b*20 (in RustCrypto we use text 'Hi' 'There')
    // Use a simpler known vector:
    //   hmac_sha256("Hi There", key=0x0b × 20) → RFC test 1
    // For simplicity here use PG's own example.
    //   hmac_sha256('The quick brown fox jumps over the lazy dog', 'key')
    //   = f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
    let v = first(
        &mut e,
        "SELECT hmac('The quick brown fox jumps over the lazy dog', 'key', 'sha256')",
    );
    assert_eq!(
        to_hex(&v),
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[test]
fn hmac_sha1_known_vector() {
    let mut e = Engine::new();
    // hmac_sha1('The quick brown fox jumps over the lazy dog', 'key')
    // = de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9
    let v = first(
        &mut e,
        "SELECT hmac('The quick brown fox jumps over the lazy dog', 'key', 'sha1')",
    );
    assert_eq!(to_hex(&v), "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9");
}

#[test]
fn hmac_all_algos_produce_correct_size() {
    let mut e = Engine::new();
    for (algo, size) in &[
        ("md5", 16usize),
        ("sha1", 20),
        ("sha224", 28),
        ("sha256", 32),
        ("sha384", 48),
        ("sha512", 64),
    ] {
        let sql = format!("SELECT hmac('data', 'key', '{algo}')");
        match first(&mut e, &sql) {
            spg_storage::Value::Bytes(b) => {
                assert_eq!(b.len(), *size, "{algo}: expected {size} bytes")
            }
            other => panic!("{algo}: {other:?}"),
        }
    }
}

#[test]
fn hmac_unsupported_algo_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT hmac('data', 'key', 'blake2b')").is_err());
}

#[test]
fn hmac_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT hmac(NULL::text, 'key', 'sha256')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT hmac('data', NULL::text, 'sha256')"),
        spg_storage::Value::Null
    ));
}
