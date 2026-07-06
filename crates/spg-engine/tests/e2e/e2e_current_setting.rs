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
    // SHOW and pg_settings agree with the function (drivers gate feature
    // use on server_version_num; live PG18.4 = 180004).
    assert_eq!(
        text(&first(&mut e, "SHOW server_version_num")),
        "180004"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT setting FROM pg_settings WHERE name = 'server_version_num'"
        )),
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

#[test]
fn custom_namespaced_guc_round_trips() {
    // Apps stash request context in custom GUCs and read it back with
    // current_setting for RLS (`SET app.user_id = '42'` →
    // current_setting('app.user_id') = '42'). Verified vs live PG18.4.
    let mut e = Engine::new();
    e.execute("SET app.user_id = '42'").unwrap();
    assert_eq!(text(&first(&mut e, "SELECT current_setting('app.user_id')")), "42");
    // Two-segment namespace survives (the qualifier is NOT stripped as a
    // schema would be).
    e.execute("SET myapp.tenant = 'acme'").unwrap();
    assert_eq!(text(&first(&mut e, "SELECT current_setting('myapp.tenant')")), "acme");
    // A SET value wins over the static default for a standard GUC too.
    e.execute("SET application_name = 'reports'").unwrap();
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('application_name')")),
        "reports"
    );
    // Unknown custom GUC with missing_ok = true → NULL (PG).
    assert!(matches!(
        first(&mut e, "SELECT current_setting('app.absent', true)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn set_client_encoding_rejects_non_utf8() {
    // v7.38 (read01) — SPG serves the wire as UTF8, so a non-UTF8
    // client_encoding is rejected rather than silently stored (which would
    // mislabel the byte stream). UTF8 / UNICODE (and utf-8 spelling) are
    // accepted; an invalid name is rejected like PG.
    let mut e = Engine::new();
    e.execute("SET client_encoding='UTF8'").unwrap();
    e.execute("SET client_encoding='utf-8'").unwrap();
    e.execute("SET client_encoding=UNICODE").unwrap();
    e.execute("SET client_encoding='UTF8'").unwrap();
    for bad in ["SET client_encoding='SJIS'", "SET client_encoding='LATIN1'", "SET client_encoding='BOGUS'"] {
        assert!(e.execute(bad).is_err(), "should reject: {bad}");
    }
    // A rejected SET leaves the prior (UTF8) value in place, and other
    // GUCs are unaffected.
    assert_eq!(text(&first(&mut e, "SELECT current_setting('client_encoding')")), "UTF8");
    e.execute("SET application_name='ok'").unwrap();
}
