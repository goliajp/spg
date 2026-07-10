//! v7.37.17 (17.6 siblings) — has_any_column_privilege /
//! has_server_privilege / has_foreign_data_wrapper_privilege /
//! has_parameter_privilege / pg_has_role.

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

#[test]
fn extended_privilege_probes_return_true() {
    let mut e = Engine::new();
    for f in &[
        "has_any_column_privilege('users', 'SELECT')",
        "has_server_privilege('srv1', 'USAGE')",
        "has_foreign_data_wrapper_privilege('fdw1', 'USAGE')",
        "has_parameter_privilege('work_mem', 'SET')",
        "pg_has_role('admin', 'MEMBER')",
        "pg_has_role('admin', 'admin', 'MEMBER')",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Bool(true) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
