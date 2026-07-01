//! v7.37.17 (17.6 siblings) — WAL utility + admin action probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn wal_utility_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_walfile_name('0/1000')",
        "pg_walfile_name_offset('0/1000')",
        "pg_split_walfile_name('000000010000000000000001')",
        "pg_ls_slru()",
        "pg_ls_replslotdir()",
        "pg_switch_wal()",
        "pg_control_system()",
        "pg_control_recovery()",
        "pg_control_checkpoint()",
        "pg_control_init()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn admin_action_probes_return_true() {
    let mut e = Engine::new();
    for f in &[
        "pg_promote()",
        "pg_reload_conf()",
        "pg_rotate_logfile()",
        "pg_rotate_logfile_v2()",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Bool(true) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
