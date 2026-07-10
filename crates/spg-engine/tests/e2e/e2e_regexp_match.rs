//! v7.37.17 (17.6 siblings) — PG 10+ regexp_match (singular).

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

#[test]
fn regexp_match_first_hit() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT regexp_match('foobarbequebaz', 'bar.*que')") {
        spg_storage::Value::TextArray(items) => {
            assert_eq!(items, vec![Some("barbeque".to_string())]);
        }
        other => panic!("got {other:?}"),
    }
    // Only the FIRST match, even with later hits.
    match first(&mut e, "SELECT regexp_match('abc abc', 'abc')") {
        spg_storage::Value::TextArray(items) => {
            assert_eq!(items, vec![Some("abc".to_string())]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn regexp_match_no_hit_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT regexp_match('foobar', 'zzz')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn regexp_match_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "regexp_match(NULL::text, 'a')",
        "regexp_match('a', NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
