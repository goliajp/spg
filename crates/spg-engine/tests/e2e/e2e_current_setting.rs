//! v7.37.17 (17.6 siblings) — current_setting widened.

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
fn current_setting_server_version() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('server_version')")),
        "18.4 (SPG-compat)"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT current_setting('server_version_num')"
        )),
        "180004"
    );
    // SHOW and pg_settings agree with the function (drivers gate feature
    // use on server_version_num; live PG18.4 = 180004).
    assert_eq!(text(&first(&mut e, "SHOW server_version_num")), "180004");
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
        first(
            &mut e,
            "SELECT current_setting('bogus_unknown_param', true)"
        ),
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
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('app.user_id')")),
        "42"
    );
    // Two-segment namespace survives (the qualifier is NOT stripped as a
    // schema would be).
    e.execute("SET myapp.tenant = 'acme'").unwrap();
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('myapp.tenant')")),
        "acme"
    );
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
    for bad in [
        "SET client_encoding='SJIS'",
        "SET client_encoding='LATIN1'",
        "SET client_encoding='BOGUS'",
    ] {
        assert!(e.execute(bad).is_err(), "should reject: {bad}");
    }
    // A rejected SET leaves the prior (UTF8) value in place, and other
    // GUCs are unaffected.
    assert_eq!(
        text(&first(&mut e, "SELECT current_setting('client_encoding')")),
        "UTF8"
    );
    e.execute("SET application_name='ok'").unwrap();
}

#[test]
fn set_validates_known_typed_gucs() {
    // v7.38 (read01 P3.17) — a clearly-invalid value for a well-known typed
    // GUC errors like PG; valid values and unknown GUCs still succeed.
    let mut e = Engine::new();
    // Valid.
    for ok in [
        "SET work_mem='64MB'",
        "SET work_mem=1024",
        "SET statement_timeout='5min'",
        "SET statement_timeout=0",
        "SET enable_seqscan=off",
        "SET maintenance_work_mem='512MB'",
    ] {
        e.execute(ok).unwrap_or_else(|err| panic!("{ok}: {err:?}"));
    }
    // Invalid → error.
    for bad in [
        "SET work_mem='bogus'",
        "SET statement_timeout='notanumber'",
        "SET enable_seqscan='maybe'",
        "SET lock_timeout='abc'",
    ] {
        assert!(e.execute(bad).is_err(), "should reject: {bad}");
    }
    // v7.39 (round 501) — an unknown parameter is now REJECTED, which
    // reverses what this pin used to assert.
    //
    // It asserted acceptance "(pg_dump compat)", and that reasoning was
    // sound while SPG had no list of PG's parameter names: rejecting
    // would have refused real ones. SPG now carries all 398 (see
    // `guc_catalog`), and every `SET` in this repo's dump corpora and
    // fixtures names either one of them or a SET STATEMENT form
    // (`SET TRANSACTION` / `SESSION` / `LOCAL`) — checked, not assumed.
    // So the premise no longer holds, and matching PG18 wins:
    //
    //   PG18:  SET nonexistent_knob = 3
    //          ERROR: unrecognized configuration parameter "nonexistent_knob"
    //   SPG before round 501: SET  (accepted, and the setting the caller
    //          believed they had made was never made — round 500)
    let unknown = e.execute("SET some_random_guc='whatever'");
    assert!(unknown.is_err(), "unknown GUC should be rejected: {unknown:?}");
    // A dotted name stays accepted: PG treats it as a customised option
    // and extensions rely on that.
    e.execute("SET myapp.thing='whatever'").unwrap();
    e.execute("SET application_name='x'").unwrap();

    // Parameters PG knows but a session cannot change are refused with
    // PG's own wording, rather than accepted and ignored.
    for (sql, want) in [
        ("SET shared_buffers = 100", "without restarting the server"),
        ("SET block_size = 4096", "cannot be changed"),
        ("SET autovacuum = off", "cannot be changed now"),
    ] {
        let err = e.execute(sql);
        let msg = format!("{err:?}");
        assert!(err.is_err() && msg.contains(want), "{sql} -> {msg}");
    }
}

#[test]
fn pg_settings_has_full_17_column_shape() {
    // v7.38 (read01 P3.22) — pg_settings exposes PG 18's 17 columns with
    // accurate context / vartype / source, so admin tools can filter on
    // them. Verified vs live PG 18.4.
    let mut e = Engine::new();
    let cols = match e.execute("SELECT * FROM pg_settings").unwrap() {
        QueryResult::Rows { columns, .. } => {
            columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        }
        _ => panic!(),
    };
    assert_eq!(
        cols,
        vec![
            "name",
            "setting",
            "unit",
            "category",
            "short_desc",
            "extra_desc",
            "context",
            "vartype",
            "source",
            "min_val",
            "max_val",
            "enumvals",
            "boot_val",
            "reset_val",
            "sourcefile",
            "sourceline",
            "pending_restart",
        ]
    );
    // vartype is annotated (work_mem is integer even though shown as "4MB").
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT vartype FROM pg_settings WHERE name = 'work_mem'"
        )),
        "integer"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT context FROM pg_settings WHERE name = 'max_connections'"
        )),
        "postmaster"
    );
    // A SET marks the row source = session while boot_val stays put.
    e.execute("SET work_mem = '64MB'").unwrap();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT source FROM pg_settings WHERE name = 'work_mem'"
        )),
        "session"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT boot_val FROM pg_settings WHERE name = 'work_mem'"
        )),
        "4MB"
    );
}

#[test]
fn show_covers_more_params_and_unifies_with_pg_settings() {
    // v7.38 (read01 P3.23) — SHOW reads the same canonical GUC inventory as
    // pg_settings, so extra_float_digits / bytea_output resolve, SHOW ALL
    // lists the full set, and SET → SHOW → pg_settings agree.
    let mut e = Engine::new();
    // Previously "parameter not recognised". Values verified vs live PG 18.4.
    assert_eq!(text(&first(&mut e, "SHOW extra_float_digits")), "1");
    assert_eq!(text(&first(&mut e, "SHOW bytea_output")), "hex");
    assert_eq!(text(&first(&mut e, "SHOW server_version_num")), "180004");
    // SHOW ALL now lists the full canonical set (was a curated 13).
    let show_all = match e.execute("SHOW ALL").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!(),
    };
    assert!(
        show_all >= 30,
        "SHOW ALL should list the full set, got {show_all}"
    );
    // A SET is reflected by both SHOW and pg_settings (one store).
    e.execute("SET extra_float_digits = '3'").unwrap();
    assert_eq!(text(&first(&mut e, "SHOW extra_float_digits")), "3");
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT setting FROM pg_settings WHERE name = 'extra_float_digits'"
        )),
        "3"
    );
    // A truly unknown parameter still errors.
    assert!(e.execute("SHOW totally_bogus_param").is_err());
}
