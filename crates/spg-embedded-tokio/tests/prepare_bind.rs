//! v7.16.0 — async `prepare` + `execute_prepared` /
//! `query_prepared` round-trip. mailrs gap-eval E2: surface
//! the engine's plan cache + placeholder bind as an
//! `AsyncDatabase`-level API so the spg-sqlx adapter (E1)
//! can sit on top.

use spg_embedded_tokio::{AsyncDatabase, Value};

#[tokio::test]
async fn async_prepare_then_bind_query() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .await
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice')")
        .await
        .unwrap();
    db.execute("INSERT INTO users VALUES (2, 'bob')")
        .await
        .unwrap();

    let stmt = db
        .prepare("SELECT name FROM users WHERE id = $1")
        .await
        .unwrap();
    let rows = db
        .query_prepared(&stmt, vec![Value::Int(1)])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Text("alice".into()));

    let rows = db
        .query_prepared(&stmt, vec![Value::Int(2)])
        .await
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("bob".into()));
}

#[tokio::test]
async fn async_clone_handle_concurrent_binds() {
    // AsyncStatement is Clone — same plan can drive multiple
    // concurrent binds without a re-prepare. The Database's
    // single-writer lock still serialises the actual exec, but
    // the handle clone is O(1) Arc bump and Send.
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE items (k INT NOT NULL, v TEXT NOT NULL)")
        .await
        .unwrap();
    let insert = db
        .prepare("INSERT INTO items VALUES ($1, $2)")
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..10 {
        let db_clone = db.clone();
        let stmt_clone = insert.clone();
        handles.push(tokio::spawn(async move {
            db_clone
                .execute_prepared(
                    &stmt_clone,
                    vec![Value::Int(i), Value::Text(format!("row-{i}"))],
                )
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let rows = db
        .query("SELECT k, v FROM items ORDER BY k")
        .await
        .unwrap();
    assert_eq!(rows.len(), 10);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0], Value::Int(i as i32));
        assert_eq!(row[1], Value::Text(format!("row-{i}")));
    }
}

#[tokio::test]
async fn async_prepare_dml_persists_via_wal() {
    use std::path::PathBuf;
    let mut p: PathBuf = std::env::temp_dir();
    let nanos: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    p.push(format!("spg-async-prepare-wal-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    let path = p.join("db");

    {
        let db = AsyncDatabase::open_path(&path).await.unwrap();
        db.execute("CREATE TABLE kv (k INT NOT NULL, v TEXT NOT NULL)")
            .await
            .unwrap();
        let stmt = db
            .prepare("INSERT INTO kv VALUES ($1, $2)")
            .await
            .unwrap();
        db.execute_prepared(&stmt, vec![Value::Int(1), Value::Text("one".into())])
            .await
            .unwrap();
        db.execute_prepared(&stmt, vec![Value::Int(2), Value::Text("two".into())])
            .await
            .unwrap();
    }

    let db = AsyncDatabase::open_path(&path).await.unwrap();
    let rows = db.query("SELECT k, v FROM kv ORDER BY k").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Text("one".into())]);
    assert_eq!(rows[1], vec![Value::Int(2), Value::Text("two".into())]);

    let _ = std::fs::remove_dir_all(&p);
}
