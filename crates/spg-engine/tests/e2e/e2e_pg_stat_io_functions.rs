//! v7.37.22 (22.18 + 22.19) — pg_stat_io + pg_stat_user_functions
//! views.

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_stat_io_returns_single_aggregate_row_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_io")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "backend_type",
        "object",
        "context",
        "reads",
        "read_time",
        "writes",
        "write_time",
        "writebacks",
        "writeback_time",
        "extends",
        "extend_time",
        "op_bytes",
        "hits",
        "evictions",
        "reuses",
        "fsyncs",
        "fsync_time",
        "stats_reset",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_io missing {must}: {names:?}"
        );
    }
    assert_eq!(rows.len(), 1);
}

#[test]
fn pg_stat_user_functions_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_user_functions")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "funcid",
        "schemaname",
        "funcname",
        "calls",
        "total_time",
        "self_time",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_user_functions missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}
