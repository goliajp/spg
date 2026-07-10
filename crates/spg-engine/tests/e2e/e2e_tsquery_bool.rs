//! v7.37.17 (17.6 siblings) — tsquery_and / tsquery_or /
//! tsquery_not, the catalog forms of && / || / !!.

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
fn combinators_compose_with_matching() {
    let mut e = Engine::new();
    // AND: both lexemes required.
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_tsvector('fat cat') @@ tsquery_and(to_tsquery('fat'), to_tsquery('cat'))"
        ),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_tsvector('fat rats') @@ tsquery_and(to_tsquery('fat'), to_tsquery('cat'))"
        ),
        spg_storage::Value::Bool(false)
    ));
    // OR: either suffices.
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_tsvector('fat rats') @@ tsquery_or(to_tsquery('fat'), to_tsquery('cat'))"
        ),
        spg_storage::Value::Bool(true)
    ));
    // NOT: inverts.
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_tsvector('fat rats') @@ tsquery_not(to_tsquery('cat'))"
        ),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(
            &mut e,
            "SELECT to_tsvector('fat cat') @@ tsquery_not(to_tsquery('cat'))"
        ),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn renders_as_tsquery_text() {
    let mut e = Engine::new();
    let got = first(
        &mut e,
        "SELECT tsquery_or(to_tsquery('a'), to_tsquery('b'))::text",
    );
    let s = match got {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert!(s.contains('a') && s.contains('b') && s.contains('|'), "{s}");
}
