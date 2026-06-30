//! v7.37.22 (22.15) — `pg_catalog.pg_stat_user_indexes` view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn pg_stat_user_indexes_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_stat_user_indexes")
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "relid",
        "indexrelid",
        "schemaname",
        "relname",
        "indexrelname",
        "idx_scan",
        "idx_tup_read",
        "idx_tup_fetch",
    ] {
        assert!(
            names.contains(&must),
            "pg_stat_user_indexes missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_stat_user_indexes_lists_one_row_per_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    e.execute("CREATE INDEX ix_t_name ON t(name)").unwrap();
    e.execute("CREATE INDEX ix_t_id ON t(id)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_stat_user_indexes");
    assert_eq!(rs.len(), 2, "got {rs:?}");
    // Position 4 = indexrelname.
    let names: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[4] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(names.contains(&"ix_t_name".to_string()));
    assert!(names.contains(&"ix_t_id".to_string()));
}

#[test]
fn pg_stat_user_indexes_empty_when_no_indexes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_stat_user_indexes");
    assert!(rs.is_empty(), "got {rs:?}");
}
