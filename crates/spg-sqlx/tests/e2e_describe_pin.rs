//! v7.17.0 Phase 6.1 / 6.3 / 6.4 — sqlx integration coverage
//! status. Pin the current behavior so future commits closing
//! the gaps surface as deliberate changes.

use spg_sqlx::SpgConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Executor;

#[tokio::test]
async fn describe_returns_protocol_error_in_v7_17() {
    // Phase 6.1: Engine::describe() wired through to sqlx is a
    // v7.18 carve-out. The connector currently returns a
    // protocol error that points to offline mode.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    let r = conn.describe("SELECT 1::INT AS one").await;
    assert!(r.is_err(), "describe is stubbed in v7.17");
    let msg = format!("{:?}", r.unwrap_err());
    assert!(
        msg.contains("describe") || msg.contains("offline"),
        "expected gap-pointing error, got: {msg}"
    );
}

#[tokio::test]
async fn plain_query_through_sqlx_still_works() {
    // Negative regression: the dominant `sqlx::query("…").fetch_all`
    // shape is unaffected by the describe stub. Only the
    // compile-time `sqlx::query!()` macro needs describe; runtime
    // queries don't.
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
