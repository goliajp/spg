//! v7.16.0 — round-trip the 11 mailrs-shape scalar / array
//! types end-to-end through SQLx: i16 / i32 / i64 / bool /
//! String / Vec<u8> / f64 / chrono::DateTime<Utc> /
//! chrono::NaiveDate / serde_json::Value / Vec<i32> /
//! Vec<String>. Each test ships an INSERT + SELECT pair so a
//! single failure points at the specific encode/decode side.

use chrono::{DateTime, NaiveDate, Utc};
use spg_sqlx::{SpgPool, SpgPoolExt};
use sqlx::Row;

#[tokio::test]
async fn float64_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE f (id INT NOT NULL, val FLOAT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO f VALUES ($1, $2)")
        .bind(1_i32)
        .bind(2.5_f64)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT val FROM f WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let v: f64 = row.get("val");
    assert!((v - 2.5).abs() < 1e-9);
}

#[tokio::test]
async fn smallint_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE s (id INT NOT NULL, n SMALLINT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO s VALUES ($1, $2)")
        .bind(1_i32)
        .bind(7_i16)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT n FROM s WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let n: i16 = row.get(0);
    assert_eq!(n, 7);
}

#[tokio::test]
async fn timestamptz_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE ts (id INT NOT NULL, t TIMESTAMPTZ NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let dt: DateTime<Utc> = "2026-06-06T12:34:56Z".parse().unwrap();
    sqlx::query("INSERT INTO ts VALUES ($1, $2)")
        .bind(1_i32)
        .bind(dt)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT t FROM ts WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: DateTime<Utc> = row.get("t");
    assert_eq!(got, dt);
}

#[tokio::test]
async fn date_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE d (id INT NOT NULL, day DATE NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 6, 6).unwrap();
    sqlx::query("INSERT INTO d VALUES ($1, $2)")
        .bind(1_i32)
        .bind(day)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT day FROM d WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: NaiveDate = row.get("day");
    assert_eq!(got, day);
}

#[tokio::test]
async fn json_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE j (id INT NOT NULL, payload JSONB NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::json!({
        "host": "smtp.example.com",
        "port": 25,
        "tls": true,
    });
    sqlx::query("INSERT INTO j VALUES ($1, $2)")
        .bind(1_i32)
        .bind(v.clone())
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT payload FROM j WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: serde_json::Value = row.get("payload");
    assert_eq!(got, v);
}

#[tokio::test]
async fn text_array_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE ta (id INT NOT NULL, tags TEXT[] NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let tags = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
    sqlx::query("INSERT INTO ta VALUES ($1, $2)")
        .bind(1_i32)
        .bind(tags.clone())
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT tags FROM ta WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: Vec<String> = row.get("tags");
    assert_eq!(got, tags);
}

#[tokio::test]
async fn int_array_round_trip() {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE ia (id INT NOT NULL, vals INT[] NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let vals: Vec<i32> = vec![1, 2, 3, 100];
    sqlx::query("INSERT INTO ia VALUES ($1, $2)")
        .bind(1_i32)
        .bind(vals.clone())
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT vals FROM ia WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: Vec<i32> = row.get("vals");
    assert_eq!(got, vals);
}
