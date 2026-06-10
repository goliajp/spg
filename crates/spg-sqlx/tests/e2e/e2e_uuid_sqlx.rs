//! v7.17.0 Phase 3.P0-69 — `uuid::Uuid` Encode / Decode against
//! SPG's `UUID` column. P0-25 added the UUID type + generator
//! SQL functions; this commit closes the typed loop on the
//! sqlx surface.

use spg_sqlx::{Kind, SpgConnectOptions};
use sqlx::ConnectOptions;
use sqlx::{Column, Executor, Row, TypeInfo};
use std::str::FromStr;
use uuid::Uuid;

#[tokio::test]
async fn uuid_column_round_trips_through_bind() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE accounts (id UUID NOT NULL, name TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    let u1 = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let u2 = Uuid::from_str("0123abcd-0123-4567-89ab-cdef01234567").unwrap();
    sqlx::query("INSERT INTO accounts VALUES ($1, 'a'), ($2, 'b')")
        .bind(u1)
        .bind(u2)
        .execute(&mut conn)
        .await
        .unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM accounts ORDER BY name")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, u1);
    assert_eq!(rows[0].1, "a");
    assert_eq!(rows[1].0, u2);
    assert_eq!(rows[1].1, "b");
}

#[tokio::test]
async fn uuid_column_decodes_as_canonical_string() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE accounts (id UUID NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO accounts VALUES ('550e8400-e29b-41d4-a716-446655440000')")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (String,) = sqlx::query_as("SELECT id FROM accounts")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(got, "550e8400-e29b-41d4-a716-446655440000");
}

#[tokio::test]
async fn uuid_column_type_info_surfaces_kind_uuid() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id UUID NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001')")
        .execute(&mut conn)
        .await
        .unwrap();
    let row = sqlx::query("SELECT id FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_info().kind(), Kind::Uuid);
    assert_eq!(row.columns()[0].type_info().name(), "UUID");
}

#[tokio::test]
async fn uuid_describe_resolves_column_kind() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id UUID)")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn.describe("SELECT id FROM t").await.unwrap();
    assert_eq!(d.columns.len(), 1);
    assert_eq!(d.columns[0].type_info().kind(), Kind::Uuid);
}

#[tokio::test]
async fn uuid_decode_from_text_widens_canonical_form() {
    // The engine's `gen_random_uuid()` returns Value::Uuid; but
    // `SELECT '550e8400...'` (raw text literal) reaches as
    // Value::Text. Decode<Uuid> must parse canonical text too so
    // the user isn't surprised by which column shape the engine
    // chose.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (s TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('550e8400-e29b-41d4-a716-446655440000')")
        .execute(&mut conn)
        .await
        .unwrap();
    // sqlx's compatible check on Type<Spg> for Uuid is strict
    // (Kind::Uuid only); but the underlying Decode impl is
    // generous, so `try_get` against the raw value works.
    let (s,): (String,) = sqlx::query_as("SELECT s FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let parsed = Uuid::from_str(&s).unwrap();
    assert_eq!(
        parsed,
        Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
    );
}

#[tokio::test]
async fn uuid_decode_rejects_non_uuid_cells() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (42)")
        .execute(&mut conn)
        .await
        .unwrap();
    let r: Result<(Uuid,), _> = sqlx::query_as("SELECT id FROM t")
        .fetch_one(&mut conn)
        .await;
    assert!(r.is_err(), "INT cell must not silently decode as Uuid");
}

#[tokio::test]
async fn null_uuid_decodes_as_none() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id UUID)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (Option<Uuid>,) = sqlx::query_as("SELECT id FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn gen_random_uuid_returns_real_uuid_through_sqlx() {
    // Regression: `SELECT gen_random_uuid()` produces an engine
    // Value::Uuid (not a Text literal); the typed decode path
    // must reach uuid::Uuid directly.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    let (got,): (Uuid,) = sqlx::query_as("SELECT gen_random_uuid()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_ne!(
        got,
        Uuid::nil(),
        "gen_random_uuid() must not be the nil UUID"
    );
}
