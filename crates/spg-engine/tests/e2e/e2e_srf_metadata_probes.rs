//! v7.37.17 (17.6 siblings) — SRF settings/metadata reader probes:
//! pg_get_keywords / pg_options_to_table / pg_timezone_names /
//! pg_partition_tree / etc.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn srf_metadata_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_get_keywords()",
        "pg_options_to_table('{}'::text)",
        "pg_show_all_settings()",
        "pg_show_all_file_settings()",
        "pg_timezone_names()",
        "pg_timezone_abbrevs()",
        "pg_partition_tree('t')",
        "pg_mcv_list_items('x'::bytea)",
        "pg_ls_logicalmapdir()",
        "pg_ls_logicalsnapdir()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
