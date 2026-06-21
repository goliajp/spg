//! v7.10.4–6 — BYTEA / BYTES end-to-end. Epic 1 of v7.10.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value<'static> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn bytea_round_trip_hex_literal() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x48656c6c6f')");
    let v = select_value(&mut eng, "SELECT b FROM t");
    let Value::Bytes(bytes) = v else {
        panic!("expected Bytes");
    };
    assert_eq!(bytes.as_ref(), b"Hello");
}

#[test]
fn bytes_alias_works() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTES NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\xDEADBEEF')");
    let v = select_value(&mut eng, "SELECT b FROM t");
    let Value::Bytes(bytes) = v else {
        panic!("expected Bytes");
    };
    assert_eq!(bytes, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn bytea_escape_form() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    // \\000 = NUL byte; 'hi\\000there' decodes to "hi\0there".
    ok(&mut eng, "INSERT INTO t VALUES ('hi\\000there')");
    let v = select_value(&mut eng, "SELECT b FROM t");
    let Value::Bytes(bytes) = v else {
        panic!("expected Bytes");
    };
    assert_eq!(bytes.as_ref(), b"hi\0there");
}

#[test]
fn bytea_uppercase_hex_accepted() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\xDEadBE10')");
    let v = select_value(&mut eng, "SELECT b FROM t");
    let Value::Bytes(bytes) = v else {
        panic!("expected Bytes");
    };
    assert_eq!(bytes, vec![0xDE, 0xAD, 0xBE, 0x10]);
}

#[test]
fn octet_length_on_bytea() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x48656c6c6f')");
    let v = select_value(&mut eng, "SELECT OCTET_LENGTH(b) FROM t");
    assert!(matches!(v, Value::Int(5)));
}

#[test]
fn length_on_bytea_returns_byte_count() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES ('\\x00010203')");
    let v = select_value(&mut eng, "SELECT LENGTH(b) FROM t");
    assert!(matches!(v, Value::Int(4)));
}

#[test]
fn odd_length_hex_rejected() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (b BYTEA NOT NULL)");
    let err = eng.execute("INSERT INTO t VALUES ('\\xABC')").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("BYTEA") || msg.contains("hex"), "{msg}");
}

#[test]
fn bytea_persists_across_snapshot() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE t (id INT NOT NULL, b BYTEA NOT NULL)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1, '\\xDEADBEEF')");
    ok(&mut eng, "INSERT INTO t VALUES (2, '\\xCAFE')");
    let bytes = eng.snapshot();
    let mut eng2 = Engine::restore_envelope(&bytes).expect("reload");
    let v = select_value(&mut eng2, "SELECT b FROM t WHERE id = 1");
    let Value::Bytes(b) = v else { panic!() };
    assert_eq!(b, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}
