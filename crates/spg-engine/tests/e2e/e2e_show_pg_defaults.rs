//! v7.37.17 (17.6 siblings) — SHOW <param> returns a PG-compatible
//! default for common driver-probed GUCs that SPG doesn't yet
//! honor at the engine level.

use spg_engine::{Engine, QueryResult};

fn show(e: &mut Engine, param: &str) -> String {
    let r = e
        .execute(&format!("SHOW {param}"))
        .unwrap_or_else(|err| panic!("SHOW {param}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("unexpected value {other:?}"),
    }
}

#[test]
fn show_pg_default_timeouts() {
    let mut e = Engine::new();
    assert_eq!(show(&mut e, "lock_timeout"), "0");
    assert_eq!(show(&mut e, "idle_in_transaction_session_timeout"), "0");
    assert_eq!(show(&mut e, "transaction_timeout"), "0");
}

#[test]
fn show_pg_default_gucs() {
    let mut e = Engine::new();
    assert_eq!(show(&mut e, "client_min_messages"), "notice");
    assert_eq!(show(&mut e, "default_tablespace"), "");
    assert_eq!(show(&mut e, "default_table_access_method"), "heap");
    assert_eq!(show(&mut e, "row_security"), "on");
    assert_eq!(show(&mut e, "check_function_bodies"), "on");
    assert_eq!(show(&mut e, "xmloption"), "content");
}

#[test]
fn show_memory_and_connection_gucs() {
    let mut e = Engine::new();
    assert_eq!(show(&mut e, "work_mem"), "4MB");
    assert_eq!(show(&mut e, "maintenance_work_mem"), "64MB");
    assert_eq!(show(&mut e, "max_connections"), "100");
    assert_eq!(show(&mut e, "shared_buffers"), "128MB");
    assert_eq!(show(&mut e, "effective_cache_size"), "4GB");
}

#[test]
fn set_then_show_reflects_session_value() {
    let mut e = Engine::new();
    e.execute("SET lock_timeout = '500ms'").unwrap();
    assert_eq!(show(&mut e, "lock_timeout"), "500ms");
    e.execute("SET work_mem = '32MB'").unwrap();
    assert_eq!(show(&mut e, "work_mem"), "32MB");
}
