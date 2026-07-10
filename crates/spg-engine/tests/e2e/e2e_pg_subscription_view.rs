//! v7.37.21 (21.13-c) — `pg_catalog.pg_subscription` view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn pg_subscription_has_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_subscription")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "subdbid",
        "subname",
        "subowner",
        "subenabled",
        "subconninfo",
        "subslotname",
        "subpublications",
        "subbinary",
        "substream",
    ] {
        assert!(
            names.contains(&must),
            "pg_subscription missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_subscription_empty_when_no_subscriptions() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_subscription");
    assert!(rs.is_empty(), "got {rs:?}");
}

#[test]
fn pg_subscription_subconninfo_is_always_redacted() {
    // Walk every row through CREATE SUBSCRIPTION (even though
    // SPG's syntax differs slightly) and verify subconninfo is
    // never the original connection string — credential safety
    // is a hard invariant.
    let mut e = Engine::new();
    e.execute(
        "CREATE SUBSCRIPTION sub_a \
         CONNECTION 'host=upstream port=5432 user=postgres password=secret123' \
         PUBLICATION p_all",
    )
    .unwrap_or_else(|err| panic!("CREATE SUBSCRIPTION: {err:?}"));
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_subscription");
    assert_eq!(rs.len(), 1);
    // Position 5 = subconninfo.
    if let Value::Text(s) = &rs[0][5] {
        assert!(
            !s.contains("secret123"),
            "subconninfo leaked password: {s:?}"
        );
        assert!(
            s == "[redacted]",
            "subconninfo should be the redact-sentinel: {s:?}"
        );
    } else {
        panic!("subconninfo wrong type");
    }
}
