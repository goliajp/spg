//! v7.37.22 (22.1) — `pg_stat_statements` PG-shape compatibility.
//! Dashboards / regression tools query specific PG columns;
//! verify the column set + a populated row after running a few
//! SELECTs.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[test]
fn pg_stat_statements_columns_match_pg() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_stat_statements")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    // PG-canonical column names. The exact set + order matters
    // for tools doing positional access; verify the names appear
    // in the expected positions.
    let want = [
        "userid",
        "dbid",
        "toplevel",
        "queryid",
        "query",
        "plans",
        "total_plan_time",
        "min_plan_time",
        "max_plan_time",
        "mean_plan_time",
        "stddev_plan_time",
        "calls",
        "total_exec_time",
        "min_exec_time",
        "max_exec_time",
        "mean_exec_time",
        "stddev_exec_time",
        "rows",
        "shared_blks_hit",
        "shared_blks_read",
        "shared_blks_dirtied",
        "shared_blks_written",
        "local_blks_hit",
        "local_blks_read",
        "local_blks_dirtied",
        "local_blks_written",
        "temp_blks_read",
        "temp_blks_written",
        "blk_read_time",
        "blk_write_time",
        "wal_records",
        "wal_fpi",
        "wal_bytes",
        "jit_functions",
        "jit_generation_time",
        "jit_inlining_count",
        "jit_inlining_time",
        "jit_emission_count",
    ];
    assert_eq!(
        columns.len(),
        want.len(),
        "column count mismatch: got {}, want {}",
        columns.len(),
        want.len()
    );
    for (i, expected) in want.iter().enumerate() {
        assert_eq!(
            columns[i].name.as_str(),
            *expected,
            "column position {i} differs"
        );
    }
}

/// Monotonic counter clock for tests — query_stats only records
/// when the engine has a clock attached. Each tick returns
/// `n * 1000` micros so elapsed > 0 on every statement.
fn tick_clock() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static C: AtomicI64 = AtomicI64::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    n * 1000
}

#[test]
fn pg_stat_statements_rows_get_populated_after_select() {
    let mut e = Engine::new().with_clock(tick_clock);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    // Run a couple of distinct queries — they should land in the
    // registry.
    e.execute("SELECT * FROM t").unwrap();
    e.execute("SELECT COUNT(*) FROM t").unwrap();
    let r = e
        .execute("SELECT * FROM pg_stat_statements")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert!(
        !rows.is_empty(),
        "expected pg_stat_statements to be populated"
    );
    // Every row should have toplevel=true, userid=10, dbid=16384
    // (the PG-default mapping documented in spg_admin.rs).
    for row in &rows {
        assert!(matches!(row.values[0], Value::BigInt(10)), "userid");
        assert!(matches!(row.values[1], Value::BigInt(16384)), "dbid");
        assert!(matches!(row.values[2], Value::Bool(true)), "toplevel");
        // calls >= 1
        if let Value::BigInt(n) = row.values[11] {
            assert!(n >= 1, "calls should be ≥1");
        } else {
            panic!("calls column wrong type");
        }
    }
}
