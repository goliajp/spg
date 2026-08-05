//! P0-6 second cut — is the group-commit queue the cost of a big batch?
//!
//! The engine runs the panel's 1000-row VALUES insert in 0.873 ms while the
//! same statement over pgwire takes 6.71 ms (PG18: 2.16 ms), so ~5.8 ms is
//! server path. `handle_execute` routes a plain DML to the commit queue when
//! the connection is idle and a WAL exists; wrapping the same statement in an
//! explicit transaction takes the ordinary path instead. Same bytes, same
//! engine work, different route.
use sqlx::{AnyConnection, Connection as _, Executor as _};
use std::time::Instant;
use std::fmt::Write as _;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut s = String::with_capacity(rows as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            s.push(',');
        }
        let _ = write!(s, "({id},{},{})", id % 100, id * 7 % 100_000);
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
    for chunk in 0..50i64 {
        c.execute(batch_sql(chunk * 1000, 1000).as_str()).await?;
    }

    // 2x2: {autocommit -> group-commit queue, explicit tx -> ordinary path}
    // x {synchronous_commit on, off}. One fsync on this filesystem costs
    // ~0.5 ms (measured separately), PG pays 0.911 ms per statement and SPGS
    // 1.741 ms, so the gap is not the syscall — this says which route carries
    // it.
    let mut base = 2_000_000i64;
    let mut results: Vec<(String, f64)> = Vec::new();

    // 2x2: {autocommit -> group-commit queue, explicit tx -> ordinary path}
    // x {synchronous_commit on, off}.
    //
    // The durable commit costs SPGS 1.741 ms per statement while the fsync
    // syscall itself is ~0.5 ms on this filesystem, so ~1.24 ms is machinery
    // AROUND the fsync (PG's equivalent overhead is ~0.4 ms). A single run in
    // round 447 hinted the queue is the slower route once an fsync is in
    // play; single runs on this probe have been wrong before, so this is the
    // shape to aggregate over repetitions.
    for mode in ["on", "off"] {
        for tx in [false, true] {
            c.execute(format!("SET synchronous_commit = {mode}").as_str())
                .await?;
            for _ in 0..5 {
                c.execute(batch_sql(base, 1000).as_str()).await?;
                base += 1000;
            }
            let mut v = Vec::with_capacity(31);
            for _ in 0..31 {
                let sql = batch_sql(base, 1000);
                base += 1000;
                let t = Instant::now();
                if tx {
                    c.execute("BEGIN").await?;
                    c.execute(sql.as_str()).await?;
                    c.execute("COMMIT").await?;
                } else {
                    c.execute(sql.as_str()).await?;
                }
                v.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let route = if tx { "ordinary" } else { "queue   " };
            results.push((format!("sync={mode:<3} {route}"), median(v)));
        }
    }
    println!("# 1000-row VALUES insert over the wire, median of 31");
    for (k, ms) in &results {
        println!("  {k}: {ms:.3} ms");
    }
    Ok(())
}
