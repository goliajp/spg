//! v7.16.0 — SELECT decode path. mailrs-shape
//! `sqlx::query_as::<_, MyType>("SELECT …").fetch_one(&pool)`
//! coverage starts here; this file exercises raw rows + the
//! built-in scalar decoders so the dependency graph for derive
//! (FromRow) is grounded.

use spg_sqlx::{SpgPool, SpgPoolExt};
use sqlx::Row;

#[tokio::test]
async fn fetch_one_scalar_int() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ($1, $2)")
        .bind(1_i32)
        .bind("alice")
        .execute(&pool)
        .await
        .unwrap();

    let row = sqlx::query("SELECT id, name FROM t WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let id: i32 = row.get(0);
    let name: String = row.get(1);
    assert_eq!(id, 1);
    assert_eq!(name, "alice");
}

#[tokio::test]
async fn fetch_one_by_name() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query(
        "CREATE TABLE u (uid BIGINT NOT NULL, handle TEXT NOT NULL, active BOOLEAN NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO u VALUES ($1, $2, $3)")
        .bind(42_i64)
        .bind("bob")
        .bind(true)
        .execute(&pool)
        .await
        .unwrap();

    let row = sqlx::query("SELECT uid, handle, active FROM u WHERE uid = $1")
        .bind(42_i64)
        .fetch_one(&pool)
        .await
        .unwrap();
    let uid: i64 = row.get("uid");
    let handle: String = row.get("handle");
    let active: bool = row.get("active");
    assert_eq!(uid, 42);
    assert_eq!(handle, "bob");
    assert!(active);
}

#[tokio::test]
async fn fetch_optional_empty() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE e (id INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let r = sqlx::query("SELECT id FROM e WHERE id = $1")
        .bind(99_i32)
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert!(r.is_none());
}

#[tokio::test]
async fn fetch_all_three_rows() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE k (n INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    for n in [1_i32, 2, 3] {
        sqlx::query("INSERT INTO k VALUES ($1)")
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
    }

    let rows = sqlx::query("SELECT n FROM k ORDER BY n")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    let ns: Vec<i32> = rows.iter().map(|r| r.get::<i32, _>(0)).collect();
    assert_eq!(ns, vec![1, 2, 3]);
}

#[tokio::test]
async fn fetch_bytes_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE b (id INT NOT NULL, blob BYTEA NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let payload: Vec<u8> = b"\x00\x01\x02hello".to_vec();
    sqlx::query("INSERT INTO b VALUES ($1, $2)")
        .bind(1_i32)
        .bind(payload.clone())
        .execute(&pool)
        .await
        .unwrap();

    let row = sqlx::query("SELECT blob FROM b WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: Vec<u8> = row.get("blob");
    assert_eq!(got, payload);
}
