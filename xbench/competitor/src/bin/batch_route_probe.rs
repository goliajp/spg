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
    for chunk in 0..50i64 {
        c.execute(batch_sql(chunk * 1000, 1000).as_str()).await?;
    }

    let mut on = Vec::new();
    let mut off = Vec::new();
    let mut base = 2_000_000i64;
    for mode in ["on", "off"] {
        c.execute(format!("SET synchronous_commit = {mode}").as_str())
            .await?;
        // Warm.
        for _ in 0..5 {
            c.execute(batch_sql(base, 1000).as_str()).await?;
            base += 1000;
        }
        for _ in 0..31 {
            let sql = batch_sql(base, 1000);
            base += 1000;
            let t = Instant::now();
            c.execute(sql.as_str()).await?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if mode == "on" { on.push(ms) } else { off.push(ms) }
        }
    }
    let (mon, moff) = (median(on), median(off));
    println!("# 1000-row VALUES insert over the wire, median of 31");
    println!("  synchronous_commit = on  : {mon:.3} ms");
    println!("  synchronous_commit = off : {moff:.3} ms");
    println!("  delta (one fsync)        : {:.3} ms", mon - moff);
    Ok(())
}
