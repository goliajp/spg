//! v7.37.17 (17.6 siblings) — PG 9.6+ parse_ident.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn parts(v: &spg_storage::Value<'_>) -> Vec<String> {
    match v {
        spg_storage::Value::TextArray(items) => {
            items.iter().map(|o| o.clone().unwrap_or_default()).collect()
        }
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn parse_ident_bare_name() {
    let mut e = Engine::new();
    assert_eq!(parts(&first(&mut e, "SELECT parse_ident('users')")), ["users"]);
}

#[test]
fn parse_ident_qualified() {
    let mut e = Engine::new();
    assert_eq!(
        parts(&first(&mut e, "SELECT parse_ident('public.users')")),
        ["public", "users"]
    );
    assert_eq!(
        parts(&first(&mut e, "SELECT parse_ident('a.b.c')")),
        ["a", "b", "c"]
    );
}

#[test]
fn parse_ident_case_folded() {
    let mut e = Engine::new();
    // Unquoted → downcased per PG.
    assert_eq!(
        parts(&first(&mut e, "SELECT parse_ident('PublicSchema.MixedCase')")),
        ["publicschema", "mixedcase"]
    );
}

#[test]
fn parse_ident_quoted_preserves_case() {
    let mut e = Engine::new();
    assert_eq!(
        parts(&first(&mut e, r#"SELECT parse_ident('"MixedCase"."WITH.dot"')"#)),
        ["MixedCase", "WITH.dot"]
    );
}

#[test]
fn parse_ident_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT parse_ident(NULL::text)"),
        spg_storage::Value::Null
    ));
}
