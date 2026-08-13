//! r1018 (mailrs 2026-08-13 §4) — can a consumer holding an `SpgPool` ask
//! the engine what it would do?
//!
//! Their rule is that every hot-path predicate must have an execution plan
//! somebody has looked at; it exists because a 48,000-row table served 309
//! billion rows through sequential scans for months, a composite index's
//! leading column never being supplied by the predicate and nothing saying
//! so. They report they cannot follow that rule on this lane, and verify
//! their plans against PostgreSQL instead — the engine the lane is compared
//! to rather than the one it runs on.
//!
//! `spg-embedded::Database::explain` has existed since v7.36, added for the
//! same ask. It is on the wrong handle: they hold `SpgPool`, which is what
//! `connect_in_memory()` returns, and `Database` is not reachable from it.
//! So the question this file answers is the one that matters — does plain
//! `EXPLAIN` work as a query through the pool, with no new API to learn and
//! no code that is spg-specific?

use spg_sqlx::{SpgPool, SpgPoolExt};
use sqlx::Row;

async fn seeded() -> SpgPool {
    let pool: SpgPool = SpgPool::connect_in_memory().await.unwrap();
    sqlx::query(
        "CREATE TABLE outbound (id INT NOT NULL, state TEXT NOT NULL, \
         next_retry BIGINT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_outbound_retry ON outbound(next_retry)")
        .execute(&pool)
        .await
        .unwrap();
    for i in 0..200_i32 {
        sqlx::query("INSERT INTO outbound VALUES ($1, $2, $3)")
            .bind(i)
            .bind("queued")
            .bind(i64::from(i) * 60)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

/// EXPLAIN returns rows through the pool, like any other query.
#[tokio::test]
async fn explain_reaches_the_engine_through_the_pool() {
    let pool = seeded().await;
    let rows = sqlx::query("EXPLAIN SELECT id FROM outbound WHERE next_retry = 600")
        .fetch_all(&pool)
        .await
        .expect("EXPLAIN through SpgPool");
    assert!(!rows.is_empty(), "EXPLAIN returned no rows");
    let plan: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    assert!(
        plan.iter().any(|l| !l.trim().is_empty()),
        "EXPLAIN returned only blank lines: {plan:?}"
    );
}

/// The distinction their rule turns on: a predicate the index can serve
/// against one it cannot must read differently. Anything else makes the
/// plan unusable for the question they ask of it.
#[tokio::test]
async fn the_plan_distinguishes_an_index_scan_from_a_full_scan() {
    let pool = seeded().await;
    let plan_of = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sql)
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>(0))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    // Served by idx_outbound_retry.
    let indexed = plan_of("EXPLAIN SELECT id FROM outbound WHERE next_retry = 600").await;
    // No index on state.
    let scanned = plan_of("EXPLAIN SELECT id FROM outbound WHERE state = 'queued'").await;
    assert_ne!(
        indexed.to_lowercase(),
        scanned.to_lowercase(),
        "the same plan came back for an indexed predicate and an unindexed \
         one, so the plan cannot answer which one a query got:\n{indexed}"
    );
}
