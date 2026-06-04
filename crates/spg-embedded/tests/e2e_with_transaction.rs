//! v7.2.0 — `Database::with_transaction(|tx| …)`.

use spg_embedded::{Database, EngineError};
use spg_storage::Value;

#[test]
fn with_transaction_commits_on_ok() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        tx.execute("INSERT INTO t VALUES (2)")?;
        Ok::<_, EngineError>(())
    })
    .unwrap();
    let got = db.query("SELECT id FROM t WHERE id = 1").unwrap();
    assert_eq!(got.len(), 1);
    let got = db.query("SELECT id FROM t WHERE id = 2").unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn with_transaction_rolls_back_on_err() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (100)").unwrap();
    let result: Result<(), EngineError> = db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        tx.execute("INSERT INTO t VALUES (2)")?;
        // Surface a deliberate failure.
        Err(EngineError::Unsupported("rollback me".into()))
    });
    assert!(result.is_err());
    // Pre-TX row survives.
    let got = db.query("SELECT id FROM t WHERE id = 100").unwrap();
    assert_eq!(got.len(), 1);
    // In-TX rows are gone.
    for id in [1, 2] {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        assert!(got.is_empty(), "row {id} survived rollback");
    }
}

#[test]
fn with_transaction_returns_body_value() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let count = db
        .with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("INSERT INTO t VALUES (2)")?;
            tx.execute("INSERT INTO t VALUES (3)")?;
            Ok::<_, EngineError>(3usize)
        })
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn with_transaction_works_on_persistent_db() {
    let dir = std::env::temp_dir().join(format!(
        "spg-embed-tx-persist-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("INSERT INTO t VALUES (2)")?;
            Ok::<_, EngineError>(())
        })
        .unwrap();
    }
    // Reopen — committed rows survive.
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT id FROM t WHERE id = 1").unwrap();
    match &got[0][0] {
        Value::Int(1) => {}
        other => panic!("expected Int(1), got {other:?}"),
    }
}

#[test]
fn nested_with_transaction_surfaces_engine_error() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let result: Result<(), EngineError> = db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        // Inner with_transaction calls BEGIN; engine rejects
        // nested begin and bubbles up.
        tx.with_transaction(|inner| {
            inner.execute("INSERT INTO t VALUES (2)")?;
            Ok::<_, EngineError>(())
        })?;
        Ok(())
    });
    assert!(result.is_err(), "nested transactions must surface error");
}
