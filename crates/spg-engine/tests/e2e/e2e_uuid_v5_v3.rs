//! v7.37.17 (17.6 siblings) — uuid-ossp uuid_generate_v5/v3 +
//! namespace constants + uuid_nil.

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

fn uuid_bytes(v: &spg_storage::Value<'_>) -> [u8; 16] {
    match v {
        spg_storage::Value::Uuid(b) => *b,
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn uuid_nil_all_zeros() {
    let mut e = Engine::new();
    assert_eq!(uuid_bytes(&first(&mut e, "SELECT uuid_nil()")), [0u8; 16]);
}

#[test]
fn namespace_constants_match_rfc() {
    let mut e = Engine::new();
    // RFC 4122 Appendix C: DNS namespace = 6ba7b810-9dad-11d1-80b4-00c04fd430c8
    let dns = uuid_bytes(&first(&mut e, "SELECT uuid_ns_dns()"));
    assert_eq!(dns[0], 0x6b);
    assert_eq!(dns[3], 0x10);
    // URL namespace differs in byte 3: ...11.
    let url = uuid_bytes(&first(&mut e, "SELECT uuid_ns_url()"));
    assert_eq!(url[3], 0x11);
}

#[test]
fn uuid_v5_deterministic_and_known_vector() {
    let mut e = Engine::new();
    // uuid_generate_v5(uuid_ns_dns(), 'www.example.com')
    // = 2ed6657d-e927-568b-95e1-2665a8aea6a2 (RFC 4122 well-known)
    let b = uuid_bytes(&first(
        &mut e,
        "SELECT uuid_generate_v5(uuid_ns_dns(), 'www.example.com')",
    ));
    let expected: [u8; 16] = [
        0x2e, 0xd6, 0x65, 0x7d, 0xe9, 0x27, 0x56, 0x8b, 0x95, 0xe1, 0x26, 0x65, 0xa8, 0xae, 0xa6,
        0xa2,
    ];
    assert_eq!(b, expected);
    // Deterministic across calls.
    let b2 = uuid_bytes(&first(
        &mut e,
        "SELECT uuid_generate_v5(uuid_ns_dns(), 'www.example.com')",
    ));
    assert_eq!(b, b2);
}

#[test]
fn uuid_v3_version_and_deterministic() {
    let mut e = Engine::new();
    let a = uuid_bytes(&first(
        &mut e,
        "SELECT uuid_generate_v3(uuid_ns_dns(), 'test')",
    ));
    assert_eq!(a[6] >> 4, 3, "version nibble should be 3");
    assert_eq!(a[8] >> 6, 0b10, "variant bits should be 10");
    let b = uuid_bytes(&first(
        &mut e,
        "SELECT uuid_generate_v3(uuid_ns_dns(), 'test')",
    ));
    assert_eq!(a, b);
}

#[test]
fn uuid_v5_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT uuid_generate_v5(NULL::uuid, 'x')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT uuid_generate_v5(uuid_ns_dns(), NULL::text)"),
        spg_storage::Value::Null
    ));
}
