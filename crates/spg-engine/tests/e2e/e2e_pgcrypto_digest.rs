//! v7.37.17 (17.6 siblings) — pgcrypto digest(data, type) — real
//! hash dispatch through md5/sha1/sha2 family.

use std::fmt::Write;

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
        spg_storage::Value::Bytes(b) => {
            let mut out = String::with_capacity(b.len() * 2);
            for byte in b.iter() {
                write!(out, "{byte:02x}").expect("writing to a String cannot fail");
            }
            out
        }
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn digest_md5_matches_known_vector() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT digest('abc', 'md5')");
    assert_eq!(to_hex(&v), "900150983cd24fb0d6963f7d28e17f72");
}

#[test]
fn digest_sha256_matches_known_vector() {
    let mut e = Engine::new();
    let v = first(&mut e, "SELECT digest('abc', 'sha256')");
    assert_eq!(
        to_hex(&v),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn digest_all_algos_produce_correct_size() {
    let mut e = Engine::new();
    for (algo, size) in &[
        ("md5", 16usize),
        ("sha1", 20),
        ("sha224", 28),
        ("sha256", 32),
        ("sha384", 48),
        ("sha512", 64),
    ] {
        let sql = format!("SELECT digest('hello', '{algo}')");
        match first(&mut e, &sql) {
            spg_storage::Value::Bytes(b) => {
                assert_eq!(b.len(), *size, "{algo}: expected {size} bytes")
            }
            other => panic!("{algo}: {other:?}"),
        }
    }
}

#[test]
fn digest_unsupported_algo_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT digest('x', 'sha1024')").is_err());
    assert!(e.execute("SELECT digest('x', 'blake2b')").is_err());
}

#[test]
fn digest_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT digest(NULL::text, 'sha256')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT digest('x', NULL::text)"),
        spg_storage::Value::Null
    ));
}
