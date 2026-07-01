//! v7.37.17 (17.6 siblings) — PG reverse(text).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn reverse_ascii() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT reverse('abcdef')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "fedcba"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT reverse('')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), ""),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn reverse_multibyte() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT reverse('中文汉字')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "字汉文中"),
        other => panic!("got {other:?}"),
    }
    // Combining chars — reverse operates on code points.
    match first(&mut e, "SELECT reverse('AB')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "BA"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn reverse_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT reverse(NULL::text)"),
        spg_storage::Value::Null
    ));
}
