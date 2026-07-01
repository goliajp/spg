//! v7.37.17 (17.6 siblings) — chr/ascii/initcap PG string helpers.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn chr_ascii_roundtrip_basic() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT chr(65)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "A"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT ascii('A')") {
        spg_storage::Value::Int(65) => {}
        other => panic!("got {other:?}"),
    }
    // Chinese unicode.
    match first(&mut e, "SELECT chr(20013)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "中"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT ascii('中')") {
        spg_storage::Value::Int(20013) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn initcap_capitalizes_word_starts() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT initcap('hello world')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "Hello World"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT initcap('HeLLo WoRLd')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "Hello World"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT initcap('one-two_three.four')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "One-Two_Three.Four"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn chr_ascii_initcap_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT chr(NULL::int)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT ascii(NULL::text)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT initcap(NULL::text)"),
        spg_storage::Value::Null
    ));
}
