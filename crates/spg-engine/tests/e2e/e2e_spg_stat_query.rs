//! v6.5.1 — `spg_stat_query` virtual table.

use std::sync::atomic::{AtomicI64, Ordering};

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

// Deterministic clock for tests: monotonically incrementing by 100µs
// per call.
static CLOCK: AtomicI64 = AtomicI64::new(0);
fn clock() -> i64 {
    CLOCK.fetch_add(100, Ordering::SeqCst)
}

fn build_engine() -> Engine {
    CLOCK.store(0, Ordering::SeqCst);
    Engine::new().with_clock(clock)
}

fn rows_of(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn counter_increments_on_each_execute() {
    // v7.37.22 (22.6) — record() normalises sql before hashing.
    // `INSERT INTO t VALUES (1)` × 3 collapses to one template.
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();

    let res = eng.execute("SELECT * FROM spg_stat_query").unwrap();
    let got = rows_of(res);
    let insert_row = got
        .iter()
        .find(|r| {
            if let Value::Text(s) = &r[0] {
                s.starts_with("insert into t values")
            } else {
                false
            }
        })
        .expect("normalised INSERT row");
    assert_eq!(insert_row[1], Value::BigInt(3), "exec_count is 3");
}

#[test]
fn distinct_sql_strings_yield_separate_rows() {
    // v7.37.22 (22.6) — three INSERTs with different literals
    // collapse to ONE row (same normalised template). Structurally
    // distinct queries (DELETE, SELECT) yield distinct rows.
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 2)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 3)").unwrap();
    eng.execute("DELETE FROM t").unwrap();
    eng.execute("SELECT * FROM t").unwrap();

    let res = eng.execute("SELECT * FROM spg_stat_query").unwrap();
    let got = rows_of(res);
    let n_inserts = got
        .iter()
        .filter(|r| {
            if let Value::Text(s) = &r[0] {
                s.starts_with("insert into t values")
            } else {
                false
            }
        })
        .count();
    assert_eq!(n_inserts, 1, "three INSERTs collapse to one template");
    // The DELETE template + the SELECT template are distinct.
    let n_others = got
        .iter()
        .filter(|r| {
            if let Value::Text(s) = &r[0] {
                s.starts_with("delete") || s.starts_with("select")
            } else {
                false
            }
        })
        .count();
    assert!(n_others >= 2, "expected DELETE + SELECT templates, got {n_others}");
}

#[test]
fn columns_match_design() {
    let mut eng = build_engine();
    let res = eng.execute("SELECT * FROM spg_stat_query").unwrap();
    let columns = match res {
        QueryResult::Rows { columns, .. } => columns,
        _ => panic!("Rows"),
    };
    let names: Vec<String> = columns.into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "sql".to_string(),
            "exec_count".to_string(),
            "total_us".to_string(),
            "mean_us".to_string(),
            "max_us".to_string(),
            "last_seen_us".to_string(),
        ]
    );
}

// v7.37.7 — `pg_stat_statements` view (mailrs cascade observability
// gap closure). PG-native dashboards and tooling query
// `pg_stat_statements`; v7.37.22 (22.1) promoted the alias to a
// proper PG-shape view with 38 columns. spg_stat_query keeps its
// own simplified shape for the human-facing spgctl path.
#[test]
fn pg_stat_statements_has_pg_compatible_columns() {
    let mut eng = build_engine();
    let alias_res = eng.execute("SELECT * FROM pg_stat_statements").unwrap();
    let alias_cols = match alias_res {
        QueryResult::Rows { columns, .. } => columns,
        _ => panic!("expected Rows"),
    };
    let names: Vec<&str> = alias_cols.iter().map(|c| c.name.as_str()).collect();
    // PG-canonical column names that dashboards depend on.
    for must in [
        "userid", "dbid", "query", "calls", "total_exec_time",
        "max_exec_time", "mean_exec_time", "queryid",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_statements missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_stat_statements_records_executions() {
    // v7.37.22 (22.6) — record() normalises sql; the surface
    // shows the template not the original. Two INSERTs with
    // the literal 42 collapse to the same template row.
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (42)").unwrap();
    eng.execute("INSERT INTO t VALUES (42)").unwrap();

    let res = eng.execute("SELECT * FROM pg_stat_statements").unwrap();
    let got = rows_of(res);
    // Column positions per v7.37.22 (22.1):
    //   4=query, 11=calls
    let insert_row = got
        .iter()
        .find(|r| {
            if let Value::Text(s) = &r[4] {
                s.starts_with("insert into t values")
            } else {
                false
            }
        })
        .expect("pg_stat_statements must surface the INSERT template");
    assert_eq!(insert_row[11], Value::BigInt(2), "calls should be 2");
}

#[test]
fn elapsed_increases_on_repeat_recording() {
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    for _ in 0..5 {
        eng.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let snap = eng.query_stats().get("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(snap.exec_count, 5);
    // total_us increases monotonically; max ≥ mean.
    assert!(snap.total_us >= snap.max_us);
    assert!(snap.max_us >= snap.total_us / snap.exec_count);
}
