//! v7.37.22 (22.20 + 22.21 + 22.22) — pg_stat_progress_*
//! views (vacuum, create_index, analyze).

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_stat_progress_vacuum_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_progress_vacuum")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "pid",
        "datid",
        "datname",
        "relid",
        "phase",
        "heap_blks_total",
        "heap_blks_scanned",
        "heap_blks_vacuumed",
        "index_vacuum_count",
        "max_dead_tuples",
        "num_dead_tuples",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_progress_vacuum missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}

#[test]
fn pg_stat_progress_create_index_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_progress_create_index")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "pid",
        "datid",
        "datname",
        "relid",
        "index_relid",
        "command",
        "phase",
        "lockers_total",
        "lockers_done",
        "current_locker_pid",
        "blocks_total",
        "blocks_done",
        "tuples_total",
        "tuples_done",
        "partitions_total",
        "partitions_done",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_progress_create_index missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}

#[test]
fn pg_stat_progress_analyze_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_progress_analyze")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "pid",
        "datid",
        "datname",
        "relid",
        "phase",
        "sample_blks_total",
        "sample_blks_scanned",
        "ext_stats_total",
        "ext_stats_computed",
        "child_tables_total",
        "child_tables_done",
        "current_child_table_relid",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_progress_analyze missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}
