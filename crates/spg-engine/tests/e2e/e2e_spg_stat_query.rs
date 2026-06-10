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

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn counter_increments_on_each_execute() {
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();

    let res = eng.execute("SELECT * FROM spg_stat_query").unwrap();
    let got = rows_of(res);
    let insert_row = got
        .iter()
        .find(|r| r[0] == Value::Text("INSERT INTO t VALUES (1)".to_string()))
        .expect("INSERT row");
    assert_eq!(insert_row[1], Value::BigInt(3), "exec_count is 3");
}

#[test]
fn distinct_sql_strings_yield_separate_rows() {
    let mut eng = build_engine();
    eng.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 2)").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 3)").unwrap();

    let res = eng.execute("SELECT * FROM spg_stat_query").unwrap();
    let got = rows_of(res);
    let n_inserts = got
        .iter()
        .filter(|r| {
            if let Value::Text(s) = &r[0] {
                s.starts_with("INSERT INTO t")
            } else {
                false
            }
        })
        .count();
    assert_eq!(n_inserts, 3, "each distinct INSERT SQL has its own row");
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
