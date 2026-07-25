//! v7.39 (round 463) — does a bloated table slow the INSERT down?
//!
//! `wire_dead_rows` showed the panel's `insert_batch_1k` leaves 23-51k dead
//! rows on a 50k-row table on SPGS while PG18 sits at zero, and that shape
//! is the panel's largest remaining loss (1.65x). That is a correlation,
//! not a cause: per the methodology's pre-attack gate, the target has to
//! be shown to matter before anything is changed.
//!
//! So: run the same 1000-row INSERT 51 times, report EACH run's time next
//! to the dead-row count it ran against. If bloat is the cause the curve
//! rises with the count; if it is flat, the dead rows are a tax and the
//! loss is somewhere else entirely.
//!
//! `SPG_BLOAT_VACUUM=1` runs an explicit VACUUM after each cleanup delete,
//! which is the control: same statements, no bloat.

#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

use spg_bench_competitor::write_shapes::{N, batch_insert_sql, runs};
use sqlx::Executor as _;
use sqlx::any::AnyPoolOptions;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPG_WIRE_URL").map_err(|_| "set SPG_WIRE_URL".to_string())?;
    let vacuum = std::env::var("SPG_BLOAT_VACUUM").is_ok_and(|v| v == "1");
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await?;
    let mut c = pool.acquire().await?;

    let _ = c.execute("DROP TABLE IF EXISTS wb").await;
    c.execute("SET synchronous_commit = off").await?;
    c.execute("CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)")
        .await?;
    // SPG_BLOAT_NOIDX=1 drops the secondary index: VACUUM rebuilds every
    // index from scratch, so the delta between these two runs is what the
    // rebuild costs.
    if !std::env::var("SPG_BLOAT_NOIDX").is_ok_and(|v| v == "1") {
        c.execute("CREATE INDEX wb_v_idx ON wb (v)").await?;
    }
    let mut i = 1;
    while i <= N {
        c.execute(batch_insert_sql(i, 1000.min(N - i + 1)).as_str())
            .await?;
        i += 1000;
    }

    println!(
        "# round 463 — insert_batch_1k per run, vacuum={}",
        if vacuum { "on" } else { "off" }
    );
    println!("{:>4} {:>10} {:>12}", "run", "ms", "dead before");
    let mut vac_ms: Vec<f64> = Vec::new();
    let mut base = N + 1_000_000;
    let n_runs = runs();
    for r in 0..n_runs {
        let dead: i64 =
            sqlx::query_scalar("SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname='wb'")
                .fetch_one(&mut *c)
                .await
                .unwrap_or(-1);
        let sql = batch_insert_sql(base, 1000);
        let t = Instant::now();
        c.execute(sql.as_str()).await?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        // Cleanup is outside the timing window, exactly as the panel does it.
        c.execute(format!("DELETE FROM wb WHERE id >= {base} AND id < {}", base + 1000).as_str())
            .await?;
        if vacuum {
            let vt = Instant::now();
            c.execute("VACUUM wb").await?;
            vac_ms.push(vt.elapsed().as_secs_f64() * 1000.0);
        }
        // Print the head, the middle and the tail — the shape of the curve
        // is the answer, not fifty-one lines of it.
        if r < 3 || (n_runs / 2..n_runs / 2 + 2).contains(&r) || r + 3 >= n_runs {
            println!("{:>4} {:>10.3} {:>12}", r, ms, dead);
        }
        base += 10_000;
    }
    if !vac_ms.is_empty() {
        vac_ms.sort_by(f64::total_cmp);
        println!(
            "# VACUUM cost: min {:.3} ms  median {:.3} ms  max {:.3} ms",
            vac_ms[0],
            vac_ms[vac_ms.len() / 2],
            vac_ms[vac_ms.len() - 1]
        );
    }
    drop(c);
    pool.close().await;
    Ok(())
}
