//! v7.37.17 (17.6 siblings) — PG cryptographic hash functions:
//! sha1 / sha224 / sha256 / sha384 / sha512.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn to_hex(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Bytes(b) => {
            let mut s = String::new();
            for byte in b.iter() {
                s.push_str(&format!("{byte:02x}"));
            }
            s
        }
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn sha256_matches_known_hash() {
    let mut e = Engine::new();
    // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let v = first(&mut e, "SELECT sha256('')");
    assert_eq!(
        to_hex(&v),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    let v = first(&mut e, "SELECT sha256('abc')");
    assert_eq!(
        to_hex(&v),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha1_matches_known_hash() {
    let mut e = Engine::new();
    // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
    let v = first(&mut e, "SELECT sha1('abc')");
    assert_eq!(to_hex(&v), "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn sha224_384_512_produce_correct_output_len() {
    let mut e = Engine::new();
    for (fn_name, expected_bytes) in
        &[("sha224", 28usize), ("sha256", 32), ("sha384", 48), ("sha512", 64)]
    {
        let v = first(&mut e, &format!("SELECT {fn_name}('hello')"));
        match &v {
            spg_storage::Value::Bytes(b) => assert_eq!(b.len(), *expected_bytes),
            other => panic!("{fn_name}: expected Bytes, got {other:?}"),
        }
    }
}

#[test]
fn hash_null_input_returns_null() {
    let mut e = Engine::new();
    for fn_name in &["sha1", "sha224", "sha256", "sha384", "sha512"] {
        let v = first(&mut e, &format!("SELECT {fn_name}(NULL::text)"));
        assert!(
            matches!(v, spg_storage::Value::Null),
            "{fn_name}(NULL) should be NULL, got {v:?}"
        );
    }
}

#[test]
fn md5_returns_hex_text_matching_known_vector() {
    let mut e = Engine::new();
    // md5("") = d41d8cd98f00b204e9800998ecf8427e
    let v = first(&mut e, "SELECT md5('')");
    match &v {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "d41d8cd98f00b204e9800998ecf8427e"),
        other => panic!("expected Text, got {other:?}"),
    }
    // md5("abc") = 900150983cd24fb0d6963f7d28e17f72
    let v = first(&mut e, "SELECT md5('abc')");
    match &v {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "900150983cd24fb0d6963f7d28e17f72"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn md5_null_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT md5(NULL::text)"),
        spg_storage::Value::Null
    ));
}
