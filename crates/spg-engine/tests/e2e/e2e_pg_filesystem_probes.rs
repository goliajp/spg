//! v7.37.17 (17.6 siblings) — filesystem-adjacent PG probes:
//! pg_relation_filepath / pg_relation_filenode /
//! pg_ls_dir / pg_ls_waldir / pg_ls_logdir / pg_ls_tmpdir /
//! pg_read_file / pg_read_binary_file / pg_stat_file /
//! pg_get_backend_memory_contexts / pg_backend_memory_contexts.

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
fn pg_relation_filepath_and_filenode() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_relation_filepath('t')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "spg://storage"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_relation_filenode('t')") {
        spg_storage::Value::BigInt(0) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pg_ls_probes_return_null() {
    let mut e = Engine::new();
    for fn_call in &[
        "pg_ls_dir('/tmp')",
        "pg_ls_waldir()",
        "pg_ls_logdir()",
        "pg_ls_tmpdir()",
        "pg_ls_archive_statusdir()",
        "pg_read_file('/tmp/x', 0, 100)",
        "pg_read_binary_file('/tmp/x')",
        "pg_stat_file('/tmp/x')",
    ] {
        let sql = format!("SELECT {fn_call}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "{fn_call} should be NULL"
        );
    }
}

#[test]
fn pg_backend_memory_contexts_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_backend_memory_contexts()"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_backend_memory_contexts()"),
        spg_storage::Value::Null
    ));
}
