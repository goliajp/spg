//! v7.13.0 — `ALTER TABLE t ADD COLUMN …` end-to-end coverage.
//! mailrs round-5 G1 (20 migrate-*.sql hits).

use spg_engine::{Engine, QueryResult};

#[test]
fn add_column_nullable_back_fills_null() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    eng.execute("ALTER TABLE t ADD COLUMN extra TEXT").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().columns.len(), 2);
    assert_eq!(table.schema().columns[1].name, "extra");
    assert!(table.schema().columns[1].nullable);
    for row in table.rows() {
        assert_eq!(row.values.len(), 2);
        assert!(row.values[1].is_null());
    }
}

#[test]
fn add_column_without_column_keyword_is_accepted() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("ALTER TABLE t ADD extra TEXT").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().columns.len(), 2);
}

#[test]
fn add_column_with_literal_default_back_fills_existing_rows() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    eng.execute("ALTER TABLE t ADD COLUMN flag BOOL NOT NULL DEFAULT FALSE")
        .unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert!(!table.schema().columns[1].nullable);
    for row in table.rows() {
        assert!(matches!(row.values[1], spg_storage::Value::Bool(false)));
    }
}

#[test]
fn add_column_if_not_exists_is_idempotent() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t ADD COLUMN IF NOT EXISTS x TEXT")
        .unwrap();
    let r = eng.execute("ALTER TABLE t ADD COLUMN IF NOT EXISTS x TEXT");
    assert!(matches!(r, Ok(QueryResult::CommandOk { .. })));
    // Second add should be a no-op — still only one new column.
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().columns.len(), 2);
}

#[test]
fn add_column_duplicate_without_if_not_exists_errors() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    let r = eng.execute("ALTER TABLE t ADD COLUMN name TEXT");
    assert!(r.is_err());
}

#[test]
fn add_column_not_null_no_default_on_nonempty_table_errors() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = eng.execute("ALTER TABLE t ADD COLUMN flag BOOL NOT NULL");
    assert!(r.is_err(), "expected error, got {r:?}");
}

#[test]
fn add_column_not_null_no_default_on_empty_table_is_ok() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t ADD COLUMN flag BOOL NOT NULL")
        .unwrap();
}

#[test]
fn add_column_then_insert_uses_default_for_new_rows() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("ALTER TABLE t ADD COLUMN tag TEXT NOT NULL DEFAULT 'pending'")
        .unwrap();
    eng.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.rows().len(), 1);
    let v = &table.rows().get(0).unwrap().values[1];
    let s = match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert_eq!(s, "pending");
}

#[test]
fn add_column_unknown_table_errors() {
    let mut eng = Engine::new();
    let r = eng.execute("ALTER TABLE missing ADD COLUMN x TEXT");
    assert!(r.is_err());
}
