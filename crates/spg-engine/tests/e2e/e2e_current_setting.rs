//! v7.37.17 (17.6 siblings) — current_setting widened.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn current_setting_server_version() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('server_version')")),
        "18.4 (SPG-compat)"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('server_version_num')")),
        "180004"
    );
}

#[test]
fn current_setting_encoding_and_locale() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('client_encoding')")),
        "UTF8"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('lc_collate')")),
        "C.UTF-8"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('timezone')")),
        "UTC"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('search_path')")),
        "\"$user\", public"
    );
}

#[test]
fn current_setting_missing_ok_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT current_setting('bogus_unknown_param', true)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn current_setting_case_insensitive() {
    let mut e = Engine::new();
    let a = text(&first(&mut e, "SELECT current_setting('TIMEZONE')"));
    let b = text(&first(&mut e, "SELECT current_setting('timezone')"));
    assert_eq!(a, b);
    assert_eq!(a, "UTC");
}

#[test]
fn current_setting_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT current_setting(NULL::text)"),
        spg_storage::Value::Null
    ));
}
