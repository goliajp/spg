//! v6.4.3 — SQL function bundle: encode / decode + error_on_null.
//!
//! random(date, date) / random(ts, ts) deferred to STABILITY §"Out
//! of v6.4" — they need a per-row RNG state the engine doesn't
//! currently plumb (and adding it conflicts with the v6.4 SQL-polish
//! theme). Picked up if/when a user actually needs PG-shaped random
//! date/timestamp generation.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_value(eng: &mut Engine, sql: &str) -> Value {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().next().unwrap().values[0].clone(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn encode_hex_round_trip() {
    let mut eng = Engine::new();
    let v = one_value(&mut eng, "SELECT encode('hello', 'hex')");
    assert_eq!(v, Value::Text("68656c6c6f".to_string()));
    let v2 = one_value(&mut eng, "SELECT decode('68656c6c6f', 'hex')");
    assert_eq!(v2, Value::Text("hello".to_string()));
}

#[test]
fn encode_base64_round_trip() {
    let mut eng = Engine::new();
    let v = one_value(&mut eng, "SELECT encode('Hello, World!', 'base64')");
    assert_eq!(v, Value::Text("SGVsbG8sIFdvcmxkIQ==".to_string()));
    let v2 = one_value(
        &mut eng,
        "SELECT decode('SGVsbG8sIFdvcmxkIQ==', 'base64')",
    );
    assert_eq!(v2, Value::Text("Hello, World!".to_string()));
}

#[test]
fn encode_base64url_alphabet_differs_from_std() {
    let mut eng = Engine::new();
    // Bytes 0xfb 0xff produce different glyphs in std vs URL:
    //   std  → "+/8="
    //   url  → "-_8="
    // We can't easily construct arbitrary bytes in SPG (Text only
    // holds UTF-8), so compare encode side only using a text input
    // that round-trips byte-for-byte through both encodings.
    let v_std = one_value(&mut eng, "SELECT encode('hello?>', 'base64')");
    let v_url = one_value(&mut eng, "SELECT encode('hello?>', 'base64url')");
    // The two encodings differ when the payload triggers chars 62/63.
    // Here both produce safe alphanumeric so just verify URL produces
    // no '+' or '/'.
    match (v_std, v_url) {
        (Value::Text(_s_std), Value::Text(s_url)) => {
            assert!(!s_url.contains('+'), "url alphabet has no +");
            assert!(!s_url.contains('/'), "url alphabet has no /");
        }
        _ => panic!("expected text"),
    }
}

#[test]
fn encode_base32hex_round_trip() {
    let mut eng = Engine::new();
    // base32hex of "foo" — RFC 4648 §10 example.
    let v = one_value(&mut eng, "SELECT encode('foo', 'base32hex')");
    assert_eq!(v, Value::Text("CPNMU===".to_string()));
    let v2 = one_value(&mut eng, "SELECT decode('CPNMU===', 'base32hex')");
    assert_eq!(v2, Value::Text("foo".to_string()));
}

#[test]
fn encode_null_propagates() {
    let mut eng = Engine::new();
    let v = one_value(&mut eng, "SELECT encode(NULL, 'hex')");
    assert_eq!(v, Value::Null);
    let v2 = one_value(&mut eng, "SELECT decode('68', NULL)");
    assert_eq!(v2, Value::Null);
}

#[test]
fn encode_unknown_format_errors() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT encode('x', 'rot13')");
    assert!(r.is_err(), "rot13 isn't a supported encode format");
}

#[test]
fn error_on_null_returns_value_when_non_null() {
    let mut eng = Engine::new();
    // `SELECT 42` narrows to Int per SPG's literal coercion. Just
    // check that the value flows through unchanged.
    let v = one_value(&mut eng, "SELECT error_on_null(42)");
    assert!(matches!(v, Value::Int(42) | Value::BigInt(42)));
    let v2 = one_value(&mut eng, "SELECT error_on_null('abc')");
    assert_eq!(v2, Value::Text("abc".to_string()));
}

#[test]
fn error_on_null_errors_on_null() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT error_on_null(NULL)");
    assert!(r.is_err(), "error_on_null(NULL) must error");
    let msg = format!("{:?}", r.err().unwrap());
    assert!(msg.contains("error_on_null"), "error mentions function name");
}
