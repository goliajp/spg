//! Smoke test — `cargo run -p spg-bench-competitor --bin smoke`.
//!
//! Connects to each of the three competitor DBs in turn, runs
//! `SELECT 1`, prints the connection time and round-trip time. Used
//! to confirm `docker compose up` brought everything online before
//! committing to a long bench run.

use spg_bench_competitor::connection_strings;
use sqlx::any::AnyPoolOptions;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    println!("== competitor smoke test ==");
    for (label, url) in connection_strings() {
        print!("  {label:<10} → ");
        let t0 = Instant::now();
        let pool = match AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("CONNECT FAILED: {e}");
                continue;
            }
        };
        let t_connect = t0.elapsed();
        let t1 = Instant::now();
        let row: Result<(i32,), _> = sqlx::query_as("SELECT 1").fetch_one(&pool).await;
        let t_query = t1.elapsed();
        match row {
            Ok((1,)) => println!("OK (connect {t_connect:?}, query {t_query:?})"),
            Ok((v,)) => println!("UNEXPECTED VALUE: {v}"),
            Err(e) => println!("QUERY FAILED: {e}"),
        }
        pool.close().await;
    }
    Ok(())
}
