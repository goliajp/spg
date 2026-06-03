//! v6.7.0 — per-table cold_rows precise count.
//!
//! Resolves the v6.2.7 STABILITY carve-out: spg_statistic gains a
//! cold_row_count column populated by ANALYZE; spg_stat_segment
//! gains a table_name column populated by walking BTree-index Cold
//! locators.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn columns_of(res: QueryResult) -> Vec<String> {
    match res {
        QueryResult::Rows { columns, .. } => columns.into_iter().map(|c| c.name).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn spg_statistic_has_cold_row_count_column() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let cols = columns_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    assert_eq!(
        cols,
        vec![
            "table_name".to_string(),
            "column_name".to_string(),
            "null_frac".to_string(),
            "n_distinct".to_string(),
            "histogram_bounds".to_string(),
            "cold_row_count".to_string(),
        ]
    );
}

#[test]
fn spg_stat_segment_has_table_name_column() {
    let mut eng = Engine::new();
    let cols = columns_of(eng.execute("SELECT * FROM spg_stat_segment").unwrap());
    assert_eq!(
        cols,
        vec![
            "segment_id".to_string(),
            "table_name".to_string(),
            "num_rows".to_string(),
            "num_pages".to_string(),
            "total_bytes".to_string(),
        ]
    );
}

#[test]
fn analyze_populates_cold_row_count_after_freeze() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX ix_t_id ON t (id)").unwrap();
    // Insert 20 rows.
    for i in 0..20 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 'r{i}')"))
            .unwrap();
    }
    // Freeze the oldest 8 rows into a cold segment.
    let report = eng
        .freeze_oldest_to_cold("t", "ix_t_id", 8)
        .expect("freeze");
    assert_eq!(report.frozen_rows, 8);
    // Before ANALYZE, the cached count is stale (we marked it so).
    // The spg_statistic surface returns whatever is cached; the
    // stale flag is internal. ANALYZE refreshes.
    eng.execute("ANALYZE t").unwrap();
    let stats = rows_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    // At least one row per non-vector column; cold_row_count is
    // column-index 5.
    assert!(!stats.is_empty(), "ANALYZE produced spg_statistic rows");
    for row in &stats {
        assert_eq!(
            row[5],
            Value::BigInt(8),
            "expected cold_row_count = 8 across every column row"
        );
    }
}

#[test]
fn spg_stat_segment_resolves_table_name() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX ix_t ON t (id)").unwrap();
    for i in 0..10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let _ = eng.freeze_oldest_to_cold("t", "ix_t", 5).expect("freeze");

    let rows = rows_of(eng.execute("SELECT * FROM spg_stat_segment").unwrap());
    assert!(!rows.is_empty(), "expected at least one segment row");
    // table_name is column 1; should match "t" for every segment
    // that holds cold rows owned by t.
    for row in &rows {
        assert_eq!(
            row[1],
            Value::Text("t".to_string()),
            "expected table_name='t' for segment owned by t"
        );
    }
}

#[test]
fn cold_row_count_zero_before_any_freeze() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX ix_t ON t (id)").unwrap();
    for i in 0..5 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    eng.execute("ANALYZE t").unwrap();
    let stats = rows_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    for row in &stats {
        assert_eq!(row[5], Value::BigInt(0), "no freeze → cold_row_count = 0");
    }
}
