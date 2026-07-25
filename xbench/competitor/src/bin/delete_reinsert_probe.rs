//! P0-13 — which half of `delete_reinsert_1k` carries its 2.22x?
//!
//! The panel's shape times a DELETE of a 1000-row segment plus the re-insert
//! of that segment. `insert_batch_1k` — the insert alone — is 1.68x, so the
//! delete half has to be worse to push the pair to 2.22x. This times the two
//! halves separately on both engines, at synchronous_commit=off since round
//! 451 established that the durable total is not what a verdict should read.
use sqlx::{AnyConnection, Connection as _, Executor as _};
use std::time::Instant;

const N: i64 = 50_000;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut s = String::with_capacity(rows as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            s.push(',');
        }
        s.push_str(&format!("({id},{},{})", id % 100, id * 7 % 100_000));
    }
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPG_WIRE_URL").map_err(|_| "set SPG_WIRE_URL")?;
    let mut c = AnyConnection::connect(&url).await?;
    c.execute("DROP TABLE IF EXISTS wb").await?;
    c.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .await?;
    c.execute("SET synchronous_commit = off").await?;
    for chunk in 0..(N / 1000) {
        c.execute(batch_sql(chunk * 1000, 1000).as_str()).await?;
    }

    // The segment the panel churns: a 1000-row window inside the seeded data.
    let seg = 10_000i64;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1000);
    let ins = batch_sql(seg, 1000);

    // v7.39 (round 458) — `SPG_PROBE_ITERS` lengthens the timed loop so a
    // sampler has something to catch. Round 457's profile came back all
    // idle threads because the probe finished before `sample` started.
    let iters: usize = std::env::var("SPG_PROBE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(31);
    let mut dv = Vec::new();
    let mut iv = Vec::new();
    for _ in 0..5 {
        c.execute(del.as_str()).await?;
        c.execute(ins.as_str()).await?;
    }
    for _ in 0..iters {
        let t = Instant::now();
        c.execute(del.as_str()).await?;
        dv.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        c.execute(ins.as_str()).await?;
        iv.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    // v7.39 (round 459) — did the DELETE reach the index over THIS path?
    // The embedded engine seeks (round 457: 0.184 ms for 1000 rows); the
    // profile of the named connection thread is dominated by per-row
    // `eval_expr` / `binop` / `is_row_visible`, which is what a filter scan
    // looks like. These two counters say which.
    let idx: i64 = sqlx::query_scalar(
        "SELECT idx_scan FROM pg_stat_user_tables WHERE relname='wb'",
    )
    .fetch_one(&mut c)
    .await
    .unwrap_or(-1);
    let seq: i64 = sqlx::query_scalar(
        "SELECT seq_tup_read FROM pg_stat_user_tables WHERE relname='wb'",
    )
    .fetch_one(&mut c)
    .await
    .unwrap_or(-1);
    println!("# delete_reinsert_1k split, sync=off, median of {iters}");
    println!("  idx_scan={idx}  seq_tup_read={seq}");
    println!("  DELETE 1000 rows : {:.3} ms", median(dv));
    println!("  re-INSERT 1000   : {:.3} ms", median(iv));
    Ok(())
}
