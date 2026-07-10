//! v7.37.17 (17.6 siblings) — SPG-specific introspection.

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn spg_version_returns_text() {
    let mut e = Engine::new();
    let v = text(&first(&mut e, "SELECT spg_version()"));
    assert!(
        v.starts_with("SPG "),
        "expected 'SPG N.M.O' prefix, got {v:?}"
    );
}

#[test]
fn spg_edition_returns_embedded() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT spg_edition()")), "embedded");
}

#[test]
fn spg_build_time_returns_text() {
    let mut e = Engine::new();
    let v = text(&first(&mut e, "SELECT spg_build_time()"));
    assert!(!v.is_empty());
}

#[test]
fn spg_uptime_seconds_returns_zero() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT spg_uptime_seconds()") {
        spg_storage::Value::BigInt(0) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_current_edition_returns_spg_compat() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_current_edition()")),
        "SPG-compat"
    );
}
