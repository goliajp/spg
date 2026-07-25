//! v7.39 (round 462) — how much dead weight does the panel's table carry?
//!
//! Round 461 fixed the seek cap so a churned table keeps using its index.
//! What it does NOT fix is the churn itself: the server runs the reclaim in
//! a background worker on a naptime cadence, and the panel's shapes mutate
//! far faster than that cadence. This probe drives the SAME sequence the
//! panel drives, over the wire, and reads `pg_stat_user_tables` between
//! shapes so the bloat the worker leaves behind is a number rather than a
//! hypothesis.
//!
//! Run against an already-running server via `SPG_WIRE_URL`, or point it at
//! PG18 to see what the same sequence costs a reference implementation.

#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

use spg_bench_competitor::write_shapes::{N, RUNS, SHAPES, batch_insert_sql, run_shape};
use sqlx::any::AnyPoolOptions;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPG_WIRE_URL")
        .map_err(|_| "set SPG_WIRE_URL to the server to probe".to_string())?;
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await?;
    let mut conn = pool.acquire().await?;

    let rt = tokio::runtime::Handle::current();
    macro_rules! run_sql {
        ($c:expr, $sql:expr) => {{
            use sqlx::Executor as _;
            rt.block_on(async { $c.execute($sql).await.unwrap() });
        }};
    }

    tokio::task::block_in_place(|| {
        run_sql!(conn, "DROP TABLE IF EXISTS wb");
        run_sql!(conn, "SET synchronous_commit = off");
        run_sql!(
            conn,
            "CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)"
        );
        run_sql!(conn, "CREATE INDEX wb_v_idx ON wb (v)");
        let mut i = 1;
        while i <= N {
            run_sql!(conn, batch_insert_sql(i, 1000.min(N - i + 1)).as_str());
            i += 1000;
        }
    });

    let dead = |conn: &mut sqlx::pool::PoolConnection<sqlx::Any>| -> i64 {
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                sqlx::query_scalar("SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname='wb'")
                    .fetch_one(&mut **conn)
                    .await
                    .map_err(|e| eprintln!("stat read: {e}"))
                    .unwrap_or(-1)
            })
        })
    };

    println!("# round 462 — dead rows carried through the panel sequence");
    println!("# {} runs per shape, {N}-row seeded table", RUNS);
    println!("{:<22} {:>12} {:>12}", "after shape", "dead rows", "shape ms");
    println!("{:<22} {:>12} {:>12}", "(seed)", dead(&mut conn), "-");

    let mut next_base = N + 1_000_000;
    for (name, shape) in SHAPES {
        let mut total = 0.0;
        tokio::task::block_in_place(|| {
            for _ in 0..RUNS {
                total += run_shape(*shape, next_base, &mut |sql| {
                    use sqlx::Executor as _;
                    rt.block_on(async { conn.execute(sql).await.unwrap() });
                });
                next_base += 10_000;
            }
        });
        println!("{:<22} {:>12} {:>12.1}", name, dead(&mut conn), total);
    }

    tokio::task::block_in_place(|| run_sql!(conn, "DROP TABLE IF EXISTS wb"));
    drop(conn);
    pool.close().await;
    Ok(())
}
