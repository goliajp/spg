//! v7.37.17 (17.6 siblings) — pg_backup / pg_start_backup /
//! pg_stop_backup + pg_is_in_backup + pg_create_restore_point.

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
fn backup_start_stop_return_lsn_text() {
    let mut e = Engine::new();
    for f in &[
        "pg_backup_start('label')",
        "pg_start_backup('label')",
        "pg_backup_stop()",
        "pg_stop_backup()",
        "pg_create_restore_point('point1')",
    ] {
        let sql = format!("SELECT {f}");
        assert_eq!(text(&first(&mut e, &sql)), "0/0");
    }
}

#[test]
fn pg_is_in_backup_returns_false() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_is_in_backup()") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_backup_label_returns_null() {
    let mut e = Engine::new();
    for f in &["pg_backup_label()", "pg_backup_labels()"] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
