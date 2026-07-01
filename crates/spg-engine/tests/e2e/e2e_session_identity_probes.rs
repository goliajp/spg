//! v7.37.17 (17.6 siblings) — PG 16+ system_user() +
//! current_query + pg_column_summary probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn system_user_returns_auth_form_text() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT system_user()") {
        spg_storage::Value::Text(s) => {
            // PG 16 form is "auth_method:user_name" — verify shape.
            assert!(s.as_ref().contains(':'), "expected colon in {s:?}");
            assert_eq!(s.as_ref(), "trust:admin");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn current_query_returns_empty_text() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT current_query()") {
        spg_storage::Value::Text(s) => assert!(s.as_ref().is_empty()),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_current_query()") {
        spg_storage::Value::Text(s) => assert!(s.as_ref().is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_column_summary_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_column_summary('users', 'id')"),
        spg_storage::Value::Null
    ));
}
