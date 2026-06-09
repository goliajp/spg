//! v7.18 — SpgConnection per-statement snapshot routing.
//!
//! Backs the drop-in epic's sqlx Pool full-support track:
//! readonly statements outside a transaction fan out through
//! AsyncReadHandle, while writes / DDL / TX-control stay on the
//! writer mutex. The contract:
//!
//! * read-after-write within one connection: SELECT sees the
//!   INSERT we just ran on the same connection (read handle is
//!   refreshed per statement).
//! * cross-connection read-committed: after conn A commits,
//!   conn B's next SELECT sees the new row.
//! * transaction-internal queries don't take the snapshot path
//!   (the routing skips it when tx_depth > 0), so BEGIN-bracketed
//!   work sees its own uncommitted writes the way PG does.
//! * a rolled-back transaction's writes are invisible after the
//!   ROLLBACK.

use spg_sqlx::{SpgPool, SpgPoolExt};
use sqlx::{Connection, Executor, Row};

#[tokio::test]
async fn read_after_write_same_connection() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    conn.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ($1, $2)")
        .bind(1_i32)
        .bind("alice")
        .execute(&mut *conn)
        .await
        .unwrap();
    // SELECT routes through the readonly snapshot path; the
    // per-statement refresh must pick up the INSERT above.
    let row = sqlx::query("SELECT name FROM t WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let name: String = row.get(0);
    assert_eq!(name, "alice");
}

#[tokio::test]
async fn cross_connection_read_committed() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    // Writer connection seeds the schema and inserts a row.
    pool.execute("CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)")
        .await
        .unwrap();
    let mut conn_a = pool.acquire().await.unwrap();
    let mut conn_b = pool.acquire().await.unwrap();
    sqlx::query("INSERT INTO t VALUES ($1, $2)")
        .bind(7_i32)
        .bind("seven")
        .execute(&mut *conn_a)
        .await
        .unwrap();
    // conn_b's SELECT must see the row conn_a just committed.
    let row = sqlx::query("SELECT label FROM t WHERE id = $1")
        .bind(7_i32)
        .fetch_one(&mut *conn_b)
        .await
        .unwrap();
    let label: String = row.get(0);
    assert_eq!(label, "seven");
}

#[tokio::test]
async fn transaction_sees_uncommitted_writes() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    pool.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO t VALUES ($1)")
        .bind(11_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    // Inside the TX the routing must stay on the writer — the
    // snapshot path would not see uncommitted writes.
    let row = sqlx::query("SELECT id FROM t WHERE id = $1")
        .bind(11_i32)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let id: i32 = row.get(0);
    assert_eq!(id, 11);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn rolled_back_transaction_invisible_after() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    pool.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO t VALUES ($1)")
        .bind(99_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    // After the rollback, the snapshot-path SELECT must not see
    // the ghost row.
    let r = sqlx::query("SELECT id FROM t WHERE id = $1")
        .bind(99_i32)
        .fetch_optional(&mut *conn)
        .await
        .unwrap();
    assert!(r.is_none());
}

#[tokio::test]
async fn pool_concurrent_reads_dont_serialise() {
    // Pool of N connections runs N concurrent SELECTs through
    // the snapshot path. The point of this test isn't a precise
    // throughput claim (that lives in the bench in T7); it's
    // that 32 parallel reads complete cleanly with no
    // serialisation deadlock and with the right answer.
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    pool.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .await
        .unwrap();
    for i in 0..100i32 {
        sqlx::query("INSERT INTO t VALUES ($1, $2)")
            .bind(i)
            .bind(i * 7)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for i in 0..32 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let v = (i % 10) * 7;
            let row = sqlx::query("SELECT id FROM t WHERE v = $1")
                .bind(v)
                .fetch_one(&pool)
                .await
                .unwrap();
            row.get::<i32, _>(0)
        }));
    }
    for (i, task) in tasks.into_iter().enumerate() {
        let id = task.await.expect("task");
        assert_eq!(id as usize, i % 10, "task {i} got wrong row");
    }
}

#[tokio::test]
async fn write_inside_tx_visible_to_other_connection_after_commit() {
    // Combines transaction routing + cross-connection read-
    // committed: the writes are buffered by the writer engine
    // until COMMIT, and conn_b only sees them post-commit.
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    pool.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();
    let mut conn_a = pool.acquire().await.unwrap();
    let mut conn_b = pool.acquire().await.unwrap();
    let mut tx = conn_a.begin().await.unwrap();
    sqlx::query("INSERT INTO t VALUES ($1)")
        .bind(123_i32)
        .execute(&mut *tx)
        .await
        .unwrap();
    // Pre-commit: conn_b should not see the row.
    let r = sqlx::query("SELECT id FROM t WHERE id = $1")
        .bind(123_i32)
        .fetch_optional(&mut *conn_b)
        .await
        .unwrap();
    assert!(r.is_none(), "pre-commit visibility leak");
    tx.commit().await.unwrap();
    // Post-commit: conn_b's next SELECT must see it.
    let row = sqlx::query("SELECT id FROM t WHERE id = $1")
        .bind(123_i32)
        .fetch_one(&mut *conn_b)
        .await
        .unwrap();
    let id: i32 = row.get(0);
    assert_eq!(id, 123);
}
