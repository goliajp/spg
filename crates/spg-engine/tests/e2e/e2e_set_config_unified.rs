//! v7.38 (read01 P3.20) — set_config writes to the same session store as
//! SET, so set_config / SHOW / current_setting / pg_settings all agree,
//! and SHOW resolves dotted custom GUC names. Verified vs live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> Option<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => Some(s.to_string()),
            spg_storage::Value::Null => None,
            v => panic!("unexpected {v:?}"),
        },
        o => panic!("expected rows, got {o:?}"),
    }
}

#[test]
fn set_config_writes_and_unifies_all_read_paths() {
    let mut e = Engine::new();
    assert_eq!(
        scalar(&mut e, "SELECT set_config('work_mem', '128MB', false)").as_deref(),
        Some("128MB")
    );
    // All three read surfaces now agree with the write.
    assert_eq!(scalar(&mut e, "SHOW work_mem").as_deref(), Some("128MB"));
    assert_eq!(
        scalar(&mut e, "SELECT current_setting('work_mem')").as_deref(),
        Some("128MB")
    );
    // v7.39 (round 522) — the read paths agree, and PG spells the answer
    // two ways: SHOW and current_setting give the human form while
    // `pg_settings.setting` gives the bare count of the row's unit.
    // Measured: `set_config('work_mem','128MB')` → setting `131072`, kB.
    // This asserted `128MB` here, which was SPG's own spelling.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT setting FROM pg_settings WHERE name = 'work_mem'"
        )
        .as_deref(),
        Some("131072")
    );
    // A custom namespaced GUC set via set_config is visible via SHOW's
    // dotted-name form and current_setting.
    e.execute("SELECT set_config('myapp.tenant', 'acme', false)")
        .unwrap();
    assert_eq!(scalar(&mut e, "SHOW myapp.tenant").as_deref(), Some("acme"));
    assert_eq!(
        scalar(&mut e, "SELECT current_setting('myapp.tenant')").as_deref(),
        Some("acme")
    );
}

#[test]
fn set_config_local_reverts_and_validates() {
    let mut e = Engine::new();
    e.execute("SELECT set_config('work_mem', '64MB', false)")
        .unwrap();
    // is_local = true is transaction-scoped like SET LOCAL.
    e.execute("BEGIN").unwrap();
    e.execute("SELECT set_config('work_mem', '256MB', true)")
        .unwrap();
    assert_eq!(scalar(&mut e, "SHOW work_mem").as_deref(), Some("256MB"));
    e.execute("COMMIT").unwrap();
    assert_eq!(scalar(&mut e, "SHOW work_mem").as_deref(), Some("64MB"));
    // An invalid value is rejected the same way SET rejects it.
    assert!(
        e.execute("SELECT set_config('work_mem', 'bogus', false)")
            .is_err()
    );
}
