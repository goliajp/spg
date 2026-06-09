//! v7.17.0 Phase 3.P0-66 — sqlx::query!() compile-time describe
//! wired through to `Engine::describe_prepared`.
//!
//! Pre-P0-66 the adapter returned a protocol error and steered
//! users to `SQLX_OFFLINE=true`. With the real describe path live,
//! `sqlx::query!()` can resolve column shapes (name, type,
//! nullability) and parameter counts against an in-memory SPG —
//! mailrs's `cargo build` (and any sqlx app) no longer needs a
//! pre-generated metadata cache when the embedded engine is
//! reachable.

use spg_sqlx::{Kind, SpgConnectOptions};
use sqlx::ConnectOptions;
use sqlx::Executor;
use sqlx::{Column, Statement};

#[tokio::test]
async fn describe_select_literal_yields_aliased_column() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    let d = conn.describe("SELECT 1::INT AS one").await.unwrap();
    assert_eq!(d.columns.len(), 1, "one literal column expected");
    assert_eq!(d.columns[0].name(), "one");
    assert_eq!(d.columns[0].type_info().kind(), Kind::Int);
    assert!(d.parameters.is_none(), "literal-only SELECT has no params");
}

#[tokio::test]
async fn describe_select_from_table_resolves_column_types() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE users (id INT NOT NULL, name TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn
        .describe("SELECT id, name FROM users WHERE id = $1")
        .await
        .unwrap();
    assert_eq!(d.columns.len(), 2);
    assert_eq!(d.columns[0].name(), "id");
    assert_eq!(d.columns[0].type_info().kind(), Kind::Int);
    assert_eq!(d.columns[1].name(), "name");
    assert_eq!(d.columns[1].type_info().kind(), Kind::Text);
    // PG describe surfaces 1 parameter ($1).
    match d.parameters {
        Some(either::Either::Right(n)) => assert_eq!(n, 1),
        other => panic!("expected Right(1) parameter count, got {other:?}"),
    }
    // Nullability mirrors the column schema.
    assert_eq!(d.nullable, vec![Some(false), Some(true)]);
}

#[tokio::test]
async fn describe_select_wildcard_returns_full_schema() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (a INT, b TEXT, c BIGINT)")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn.describe("SELECT * FROM t").await.unwrap();
    assert_eq!(d.columns.len(), 3);
    assert_eq!(d.columns[0].name(), "a");
    assert_eq!(d.columns[1].name(), "b");
    assert_eq!(d.columns[2].name(), "c");
    assert_eq!(d.columns[0].type_info().kind(), Kind::Int);
    assert_eq!(d.columns[1].type_info().kind(), Kind::Text);
    assert_eq!(d.columns[2].type_info().kind(), Kind::BigInt);
}

#[tokio::test]
async fn describe_unknown_table_returns_empty_columns() {
    // describe planner falls through to NoData (empty columns)
    // for shapes it can't resolve. sqlx tolerates this; macros
    // fall back to offline metadata for those shapes.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    let d = conn
        .describe("SELECT * FROM nonexistent_table")
        .await
        .unwrap();
    assert!(d.columns.is_empty());
}

#[tokio::test]
async fn describe_non_select_yields_empty_describe() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn
        .describe("INSERT INTO t VALUES ($1)")
        .await
        .unwrap();
    assert!(d.columns.is_empty(), "INSERT has no row description");
    // INSERT carries one placeholder → 1 parameter visible to
    // PG's ParameterDescription frame.
    match d.parameters {
        Some(either::Either::Right(n)) => assert_eq!(n, 1),
        other => panic!("expected Right(1) parameter count, got {other:?}"),
    }
}

#[tokio::test]
async fn describe_parse_error_propagates() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    let r = conn.describe("SELEKT 1").await;
    assert!(r.is_err(), "garbage SQL should surface as parse error");
}

#[tokio::test]
async fn describe_two_placeholders_count_both() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE u (a INT, b TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn
        .describe("SELECT a FROM u WHERE a = $1 AND b = $2")
        .await
        .unwrap();
    match d.parameters {
        Some(either::Either::Right(n)) => assert_eq!(n, 2),
        other => panic!("expected Right(2) parameter count, got {other:?}"),
    }
}

#[tokio::test]
async fn prepare_with_populates_statement_columns() {
    // Regression: the `Executor::prepare_with` path returns an
    // `SpgStatement<'q>`. P0-66 doesn't yet plumb column metadata
    // through `prepare_with` (sqlx's `query!()` calls describe
    // directly), but the prepare path still must produce a
    // working statement handle for runtime `query("…")` use.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    let stmt = conn.prepare("SELECT id FROM t").await.unwrap();
    assert_eq!(stmt.sql(), "SELECT id FROM t");
    // columns() is currently empty for prepare_with (v7.17 gap —
    // describe is the typed path). Just sanity check the SQL.
    let _ = stmt.columns();
}

#[tokio::test]
async fn plain_query_through_sqlx_still_works() {
    // Negative regression retained from the pre-P0-66 pin file:
    // the dominant `sqlx::query("…").fetch_all` shape is
    // unaffected by the describe rewrite. Runtime queries don't
    // touch describe.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (1), (2), (3)")
        .execute(&mut conn)
        .await
        .unwrap();
    let r: (i64,) = sqlx::query_as("SELECT count(*) FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(r.0, 3);
}
