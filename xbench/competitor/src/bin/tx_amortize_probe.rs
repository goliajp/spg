//! P0-3 probe — does an explicit transaction amortise its fsync on the
//! EXTENDED protocol?
//!
//! Round 438 measured, over the simple protocol (psql), exactly one fsync for
//! `BEGIN` + 100 INSERTs + `COMMIT` and 100 for the autocommit equivalent —
//! the rule works there. The docker-fair panel (round 440) then showed
//! `tx_batch_100` still costing what 100 autocommit commits cost when driven
//! by sqlx, which speaks the extended protocol like every real client does.
//!
//! This drives ONE shape per run so the server's `SPG_WAL_TRACE` file can be
//! counted per phase without guessing where one shape ends.
//!
//! Run: SPG_WIRE_URL=postgres://… cargo run --release -p spg-bench-competitor \
//!        --bin tx_amortize_probe -- {tx|autocommit} [rows]

use sqlx::AnyConnection;
use sqlx::Connection as _;
use sqlx::Executor as _;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "tx".into());
    let rows: i64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let url = std::env::var("SPG_WIRE_URL")
        .map_err(|_| "set SPG_WIRE_URL to the server under test")?;

    // `pool` mode reproduces the shape that loses rows on SPGS but not on
    // PG18: every statement through `execute(&pool)`, so the connection is
    // acquired and released around each one while a raw `BEGIN` is open.
    if mode == "pool" {
        return pool_mode(&url, rows).await;
    }
    // A DEDICATED connection, not a pool: `sqlx::query(..).execute(&pool)`
    // acquires and releases per statement, which muddies "is this the
    // extended protocol?" with "what does the pool do between statements?".
    let mut c = AnyConnection::connect(&url).await?;
    c.execute("DROP TABLE IF EXISTS amort").await?;
    c.execute("CREATE TABLE amort(id INT PRIMARY KEY, v INT)").await?;
    c.execute("SET synchronous_commit = on").await?;

    let t0 = Instant::now();
    if mode == "tx" {
        c.execute("BEGIN").await?;
    }
    for k in 0..rows {
        // Parameterless text so the statement shape matches the panel's.
        c.execute(format!("INSERT INTO amort VALUES ({k}, {k})").as_str())
            .await?;
    }
    if mode == "tx" {
        c.execute("COMMIT").await?;
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM amort")
        .fetch_one(&mut c)
        .await?;
    println!("mode={mode} rows={rows} wall={ms:.1}ms stored={n}");
    Ok(())
}

/// The row-losing shape, isolated. Round 441 measured PG18 keeping all rows
/// here and SPGS keeping none, with identical client code.
async fn pool_mode(url: &str, rows: i64) -> Result<(), Box<dyn std::error::Error>> {
    use sqlx::any::AnyPoolOptions;
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS amort").execute(&pool).await?;
    sqlx::query("CREATE TABLE amort(id INT PRIMARY KEY, v INT)")
        .execute(&pool)
        .await?;
    sqlx::query("BEGIN").execute(&pool).await?;
    for k in 0..rows {
        sqlx::query(&format!("INSERT INTO amort VALUES ({k}, {k})"))
            .execute(&pool)
            .await?;
    }
    sqlx::query("COMMIT").execute(&pool).await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM amort")
        .fetch_one(&pool)
        .await?;
    println!("mode=pool rows={rows} stored={n}");
    Ok(())
}
