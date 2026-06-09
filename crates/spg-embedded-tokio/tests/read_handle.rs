//! v7.11.0-3 — AsyncReadHandle (Epic 1 of v7.11). Fan-out reader
//! pattern: snapshot isolation, no writer-lock contention.

use spg_embedded_tokio::{AsyncDatabase, EngineError, QueryResult, Value};

fn count_value(qr: QueryResult) -> i64 {
    let QueryResult::Rows { rows, .. } = qr else {
        panic!("expected Rows")
    };
    match &rows[0].values[0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[tokio::test]
async fn read_handle_returns_committed_state() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)")
        .await
        .unwrap();
    let h = db.read_handle().await;
    let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 3);
}

#[tokio::test]
async fn read_handle_snapshot_freezes_at_creation_time() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    db.execute("INSERT INTO t VALUES (1), (2)").await.unwrap();
    let h = db.read_handle().await;
    // Write past the snapshot point.
    db.execute("INSERT INTO t VALUES (3), (4)").await.unwrap();
    // Handle still sees 2 rows.
    let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 2);
}

#[tokio::test]
async fn refresh_picks_up_new_rows() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    db.execute("INSERT INTO t VALUES (1)").await.unwrap();
    let mut h = db.read_handle().await;
    let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 1);
    db.execute("INSERT INTO t VALUES (2), (3)").await.unwrap();
    h.refresh().await;
    let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 3);
}

#[tokio::test]
async fn read_handle_rejects_ddl() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    let h = db.read_handle().await;
    let err = h.query("CREATE TABLE t2 (b INT)").await.unwrap_err();
    assert!(matches!(err, EngineError::WriteRequired), "{err:?}");
}

#[tokio::test]
async fn read_handle_rejects_dml() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    let h = db.read_handle().await;
    let err = h.query("INSERT INTO t VALUES (1)").await.unwrap_err();
    assert!(matches!(err, EngineError::WriteRequired), "{err:?}");
}

#[tokio::test]
async fn many_handles_fan_out_concurrently() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    for i in 0..100i64 {
        db.execute(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }
    // Take 16 handles and fire concurrent COUNT(*) queries.
    let mut handles = Vec::new();
    for _ in 0..16 {
        let h = db.read_handle().await;
        handles.push(tokio::spawn(async move {
            let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
            count_value(r)
        }));
    }
    for h in handles {
        let count = h.await.expect("task");
        assert_eq!(count, 100);
    }
}

#[tokio::test]
async fn read_handle_does_not_block_writer() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    for i in 0..50i64 {
        db.execute(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }
    let h = db.read_handle().await;
    let counter = Arc::new(AtomicU32::new(0));
    // 32 concurrent reads against the snapshot.
    let mut read_tasks = Vec::new();
    for _ in 0..32 {
        let h_clone_ok = {
            // We don't have Clone on AsyncReadHandle, take many handles instead.
            db.read_handle().await
        };
        let c = counter.clone();
        read_tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                let _ = h_clone_ok.query("SELECT COUNT(*) FROM t").await.unwrap();
                c.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    // Writer fires concurrently with the readers.
    for i in 50..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }
    for t in read_tasks {
        t.await.expect("read task");
    }
    // 32 readers × 10 queries — every query landed.
    assert_eq!(counter.load(Ordering::Relaxed), 320);
    // Original handle still sees the 50-row snapshot.
    let r = h.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 50);
}

#[tokio::test]
async fn read_handle_after_database_clone() {
    // AsyncDatabase::clone shares the engine; read handles taken
    // from either clone see the same state.
    let db_a = AsyncDatabase::open_in_memory();
    let db_b = db_a.clone();
    db_a.execute("CREATE TABLE t (a INT NOT NULL)")
        .await
        .unwrap();
    db_a.execute("INSERT INTO t VALUES (1), (2)").await.unwrap();
    let h_b = db_b.read_handle().await;
    let r = h_b.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count_value(r), 2);
}

// v7.18 — readonly prepared/bind path on the fan-out reader.
// Backs the spg-sqlx Pool full-support track: sqlx::query!()
// goes through prepare + bind, and the SpgConnection router
// dispatches readonly statements through AsyncReadHandle so they
// don't take the writer lock.

#[tokio::test]
async fn read_handle_prepare_then_execute_prepared() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .await
        .unwrap();
    for i in 0..10i32 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 7))
            .await
            .unwrap();
    }
    let h = db.read_handle().await;
    let stmt = h.prepare("SELECT id FROM t WHERE v = $1").await.unwrap();
    let QueryResult::Rows { rows, .. } = h
        .execute_prepared(&stmt, vec![Value::Int(35)])
        .await
        .unwrap()
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(5));
}

#[tokio::test]
async fn read_handle_execute_prepared_rejects_writes() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").await.unwrap();
    let h = db.read_handle().await;
    let stmt = h.prepare("INSERT INTO t VALUES ($1)").await.unwrap();
    let err = h
        .execute_prepared(&stmt, vec![Value::Int(1)])
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::WriteRequired), "{err:?}");
}

#[tokio::test]
async fn read_handle_describe_resolves_columns() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .await
        .unwrap();
    let h = db.read_handle().await;
    let (_params, cols) = h
        .describe("SELECT id, name FROM t WHERE id = $1")
        .await
        .unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[1].name, "name");
}

#[tokio::test]
async fn read_handle_prepared_frozen_view() {
    // Same snapshot-isolation contract as `query()`: writes
    // committed after the handle was taken are invisible until
    // refresh().
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").await.unwrap();
    db.execute("INSERT INTO t VALUES (1)").await.unwrap();
    let h = db.read_handle().await;
    db.execute("INSERT INTO t VALUES (2)").await.unwrap();
    let stmt = h
        .prepare("SELECT id FROM t WHERE id = $1")
        .await
        .unwrap();
    let QueryResult::Rows { rows, .. } = h
        .execute_prepared(&stmt, vec![Value::Int(2)])
        .await
        .unwrap()
    else {
        panic!("expected Rows")
    };
    assert!(rows.is_empty(), "id=2 was inserted after snapshot");
}

#[tokio::test]
async fn read_handle_prepared_concurrent_fan_out() {
    // 16 read handles each preparing + executing concurrently —
    // no writer-lock contention since the prepared path is
    // static-on-snapshot.
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .await
        .unwrap();
    for i in 0..100i32 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 7))
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for i in 0..16 {
        let h = db.read_handle().await;
        tasks.push(tokio::spawn(async move {
            let stmt = h.prepare("SELECT id FROM t WHERE v = $1").await.unwrap();
            let v = (i % 10) * 7;
            let QueryResult::Rows { rows, .. } = h
                .execute_prepared(&stmt, vec![Value::Int(v)])
                .await
                .unwrap()
            else {
                panic!("expected Rows");
            };
            (i, rows.len())
        }));
    }
    for t in tasks {
        let (i, count) = t.await.expect("task");
        assert_eq!(count, 1, "task {i} expected 1 row");
    }
}
