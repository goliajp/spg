//! v7.37.17 (17.6 siblings) — pgcrypto gen_random_bytes + gen_salt.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn gen_random_bytes_produces_correct_size() {
    let mut e = Engine::new();
    for n in &[0u64, 1, 8, 16, 32, 64, 1024] {
        let sql = format!("SELECT gen_random_bytes({n})");
        match first(&mut e, &sql) {
            spg_storage::Value::Bytes(b) => {
                assert_eq!(b.len(), *n as usize, "n={n}: got {} bytes", b.len())
            }
            other => panic!("n={n}: {other:?}"),
        }
    }
}

#[test]
fn gen_random_bytes_actually_random() {
    let mut e = Engine::new();
    // 32 bytes twice — should differ (not deterministic).
    let a = first(&mut e, "SELECT gen_random_bytes(32)");
    let b = first(&mut e, "SELECT gen_random_bytes(32)");
    let (spg_storage::Value::Bytes(ab), spg_storage::Value::Bytes(bb)) = (&a, &b)
    else {
        panic!("expected Bytes");
    };
    assert_ne!(
        ab.as_ref(),
        bb.as_ref(),
        "consecutive gen_random_bytes(32) calls produced same bytes"
    );
}

#[test]
fn gen_random_bytes_cap_and_negative_errors() {
    let mut e = Engine::new();
    // Exceeds 1024 byte cap.
    assert!(e.execute("SELECT gen_random_bytes(2048)").is_err());
    // Negative.
    assert!(e.execute("SELECT gen_random_bytes(-1)").is_err());
}

#[test]
fn gen_random_bytes_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT gen_random_bytes(NULL::int)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn gen_salt_returns_stub_text() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT gen_salt('bf')") {
        spg_storage::Value::Text(s) => {
            assert!(!s.is_empty());
        }
        other => panic!("got {other:?}"),
    }
}
