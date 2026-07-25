//! v7.39 (round 462) — what does Describe say the result columns are?
//!
//! Found while instrumenting the churn panel: sqlx reading
//! `pg_stat_user_tables` gets a row with ZERO columns — the DataRow carries
//! the value, but the RowDescription the extended protocol handed back
//! declares nothing, so every `row.get(0)` is out of bounds. psql's simple
//! query path is unaffected, which is why this never showed up in the
//! differential probes.
//!
//! This probe drives the same statements over the extended protocol and
//! prints how many columns each one declares, so the boundary between
//! "works" and "declares nothing" is a measurement.

#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

use sqlx::{Column as _, Executor as _, Row as _};
use std::time::Duration;

const PROBES: &[(&str, &str)] = &[
    ("plain literal", "SELECT 1::bigint AS a"),
    ("user table", "SELECT id FROM wb"),
    ("user table star", "SELECT * FROM wb"),
    ("user view", "SELECT id FROM wv"),
    ("user view star", "SELECT * FROM wv"),
    ("join", "SELECT wb.id FROM wb JOIN wb b2 ON wb.id = b2.id"),
    ("subquery from", "SELECT id FROM (SELECT id FROM wb) s"),
    ("union", "SELECT id FROM wb UNION SELECT id FROM wb"),
    ("cte", "WITH c AS (SELECT id FROM wb) SELECT id FROM c"),
    ("agg", "SELECT count(*) AS n FROM wb"),
    ("pg_stat_user_tables", "SELECT n_dead_tup FROM pg_stat_user_tables"),
    ("pg_stat_user_tables *", "SELECT * FROM pg_stat_user_tables"),
    ("pg_class", "SELECT relname FROM pg_class"),
    ("pg_tables", "SELECT tablename FROM pg_tables"),
    ("pg_settings", "SELECT name FROM pg_settings"),
    ("information_schema", "SELECT table_name FROM information_schema.tables"),
    ("pg_stat_activity", "SELECT pid FROM pg_stat_activity"),
    ("pg_stat_database", "SELECT datname FROM pg_stat_database"),
    ("pg_indexes", "SELECT indexname FROM pg_indexes"),
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPG_WIRE_URL").map_err(|_| "set SPG_WIRE_URL".to_string())?;
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await?;
    let mut c = pool.acquire().await?;
    let _ = c.execute("DROP TABLE IF EXISTS wb").await;
    c.execute("CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL)")
        .await?;
    c.execute("INSERT INTO wb VALUES (1,1),(2,2)").await?;
    let _ = c.execute("DROP VIEW IF EXISTS wv").await;
    c.execute("CREATE VIEW wv AS SELECT id, g FROM wb").await?;

    println!("# round 462 — columns declared over the EXTENDED protocol");
    println!(
        "{:<24} {:>8} {:>8}  first column name",
        "query", "cols", "rows"
    );
    for (name, sql) in PROBES {
        match sqlx::query(sql).fetch_all(&mut *c).await {
            Ok(rows) => {
                let cols = rows.first().map_or(0, |r| r.columns().len());
                let first = rows
                    .first()
                    .and_then(|r| r.columns().first().map(|c| c.name().to_string()))
                    .unwrap_or_else(|| "-".into());
                println!("{:<24} {:>8} {:>8}  {}", name, cols, rows.len(), first);
            }
            Err(e) => println!("{:<24} {:>8} {:>8}  ERR {e}", name, "-", "-"),
        }
    }
    let _ = c.execute("DROP VIEW IF EXISTS wv").await;
    let _ = c.execute("DROP TABLE IF EXISTS wb").await;
    drop(c);
    pool.close().await;
    Ok(())
}
