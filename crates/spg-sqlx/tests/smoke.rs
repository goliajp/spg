//! v7.16.0 — first end-to-end test that the spg-sqlx adapter
//! drives a real sqlx call against an in-process SPG. mailrs-
//! shape: build a pool, run a DDL, INSERT bound params,
//! transaction commit/rollback.

use spg_sqlx::{SpgPool, SpgPoolExt};

#[tokio::test]
async fn pool_can_execute_ddl_and_insert_via_sqlx_query_bind() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();

    sqlx::query("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let r = sqlx::query("INSERT INTO users VALUES ($1, $2)")
        .bind(1_i32)
        .bind("alice")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(r.rows_affected(), 1);

    let r = sqlx::query("INSERT INTO users VALUES ($1, $2)")
        .bind(2_i32)
        .bind(String::from("bob"))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(r.rows_affected(), 1);
}

#[tokio::test]
async fn transaction_commit_visible() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE items (id INT NOT NULL, qty INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let r1 = sqlx::query("INSERT INTO items VALUES ($1, $2)")
        .bind(1_i32)
        .bind(10_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(r1.rows_affected(), 1);
    let r2 = sqlx::query("INSERT INTO items VALUES ($1, $2)")
        .bind(2_i32)
        .bind(20_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(r2.rows_affected(), 1);
    // Visibility within the tx — proves the engine's TX slot is
    // routed correctly through the prepared path (the bug that
    // v7.16.0 caught on `Engine::execute_prepared` skipping the
    // `current_tx` wrap).
    let in_tx_count = count_rows(tx.engine(), "items").await;
    assert_eq!(in_tx_count, 2, "rows must be visible within their own tx");
    tx.commit().await.unwrap();

    // Verify visibility by going through the embedded escape
    // hatch — SELECT decode is v7.16.x; for smoke purposes we
    // reach for the engine directly.
    let conn = pool.acquire().await.unwrap();
    let count = count_rows(conn.engine(), "items").await;
    assert_eq!(count, 2);
}

#[tokio::test]
async fn transaction_rollback_discards_inserts() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE k (id INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO k VALUES ($1)")
            .bind(1_i32)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO k VALUES ($1)")
            .bind(2_i32)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }

    let conn = pool.acquire().await.unwrap();
    let count = count_rows(conn.engine(), "k").await;
    assert_eq!(count, 0, "rollback must drop the in-flight inserts");
}

async fn count_rows(db: &spg_embedded_tokio::AsyncDatabase, table: &str) -> usize {
    let rows = db
        .query(&format!("SELECT * FROM {table}"))
        .await
        .unwrap();
    rows.len()
}
