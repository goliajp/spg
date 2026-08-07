//! v7.39 (read01 round 204) — GUC display fidelity, live-PG18
//! differential (2026-07-18):
//!   * SHOW / current_setting on an unset recognised GUC returns its
//!     boot default (was empty);
//!   * memory GUCs canonicalize to the largest binary unit at store
//!     time (`SET work_mem = '65536'` → `64MB`);
//!   * enum GUCs reject an out-of-domain value (client_min_messages).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => format!("{other:?}"),
        },
        QueryResult::CommandOk { .. } => String::new(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unset_guc_shows_boot_default() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "4MB");
    assert_eq!(
        one(&mut e, "SELECT current_setting('maintenance_work_mem')"),
        "64MB"
    );
}

#[test]
fn memory_guc_canonicalizes() {
    let mut e = Engine::new();
    e.execute("SET work_mem = '65536'").unwrap();
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "64MB");
    e.execute("SET work_mem = '64MB'").unwrap();
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "64MB");
    e.execute("SET work_mem = '1048576'").unwrap(); // 1 GiB in kB
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "1GB");
    e.execute("SET work_mem = '3072'").unwrap(); // 3 MiB
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "3MB");
}

#[test]
fn reset_returns_to_boot() {
    let mut e = Engine::new();
    e.execute("SET work_mem = '128MB'").unwrap();
    e.execute("RESET work_mem").unwrap();
    assert_eq!(one(&mut e, "SELECT current_setting('work_mem')"), "4MB");
}

#[test]
fn enum_guc_rejects_bad_value() {
    let mut e = Engine::new();
    assert!(e.execute("SET client_min_messages = warning").is_ok());
    let err = e
        .execute("SET client_min_messages = bogus")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("invalid value for parameter") && err.contains("client_min_messages"),
        "{err}"
    );
}
