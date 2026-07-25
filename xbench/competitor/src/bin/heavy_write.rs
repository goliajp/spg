//! Heavy WRITE-shape latency — SPG (embedded) vs PostgreSQL 18.
//!
//! Loss-hunt round 4: UPDATE / DELETE / batch-INSERT shapes. Every
//! timed statement runs inside BEGIN … ROLLBACK so the table state is
//! identical for every run on both engines. PG still accumulates dead
//! tuples from rolled-back work, so the PG leg VACUUMs (untimed)
//! between runs to keep later samples honest.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin heavy_write`

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use spg_bench_competitor::connection_strings;
use sqlx::AnyPool;
use sqlx::any::AnyPoolOptions;
use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;
const WARMUP: usize = 2;
const RUNS: usize = 15; // odd → clean median

/// v7.39 (round 477) — effective run count, overridable without a rebuild.
/// Same reason as `heavy`: at the default the RATIO carries enough noise to
/// make the "narrow win" shapes unrankable, and picking an attack target off
/// an unrankable list is picking noise.
fn runs() -> usize {
    std::env::var("SPG_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 3)
        .unwrap_or(RUNS)
}

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn batch_insert_sql() -> String {
    let mut sql = String::from("INSERT INTO h VALUES ");
    for i in 0..200 {
        if i > 0 {
            sql.push(',');
        }
        let id = 1_000_000 + i;
        let _ = write!(sql, "({id}, {}, {})", i % 100, val_for(id));
    }
    sql
}

fn shapes() -> Vec<(&'static str, String)> {
    vec![
        (
            "update_narrow",
            "UPDATE h SET v = v + 1 WHERE g = 50".to_string(), // 500 rows
        ),
        (
            "update_wide",
            "UPDATE h SET v = v + 1 WHERE v < 50000".to_string(), // ~25k rows
        ),
        (
            "delete_narrow",
            "DELETE FROM h WHERE g = 50".to_string(), // 500 rows
        ),
        (
            "delete_range",
            "DELETE FROM h WHERE v BETWEEN 20000 AND 40000".to_string(), // ~10k rows
        ),
        ("insert_batch200", batch_insert_sql()),
    ]
}

fn median_ms(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    let spge = bench_spg_embedded();

    let pg_url = connection_strings()
        .into_iter()
        .find(|(l, _)| *l == "postgres")
        .map(|(_, u)| u)
        .ok_or("no postgres connection string")?;
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&pg_url)
        .await?;
    let pg = bench_pg(&pool).await?;
    pool.close().await;

    println!();
    println!(
        "# heavy write shapes — median ms over {} runs (BEGIN..ROLLBACK), {N}-row table",
        runs()
    );
    println!("| shape          |   SPGE ms |   PG18 ms | SPGE/PG | verdict |");
    println!("|----------------|----------:|----------:|--------:|---------|");
    for (i, (name, _)) in shapes().iter().enumerate() {
        let s = spge[i];
        let p = pg[i];
        let ratio = s / p;
        let verdict = if ratio <= 0.95 {
            "WIN"
        } else if ratio <= 1.05 {
            "tied"
        } else if ratio < 1.20 {
            "LOSS"
        } else {
            "LOSS-P0"
        };
        println!(
            "| {:<14} | {:>9.3} | {:>9.3} | {:>6.2}× | {:<7} |",
            name, s, p, ratio, verdict
        );
    }
    println!();
    println!("# SPGS ≈ SPGE + ~15µs wire. SPGE loss ⇒ SPGS loss.");
    Ok(())
}

fn bench_spg_embedded() -> Vec<f64> {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    eng.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    for i in 1..=N {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h VALUES ({i}, {g}, {v})"))
            .unwrap();
    }
    shapes()
        .iter()
        .map(|(_, sql)| {
            for _ in 0..WARMUP {
                eng.execute("BEGIN").unwrap();
                eng.execute(sql).unwrap();
                eng.execute("ROLLBACK").unwrap();
            }
            let mut samples = Vec::with_capacity(runs());
            for _ in 0..runs() {
                eng.execute("BEGIN").unwrap();
                let t0 = Instant::now();
                eng.execute(sql).unwrap();
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
                eng.execute("ROLLBACK").unwrap();
            }
            median_ms(samples)
        })
        .collect()
}

async fn bench_pg(pool: &AnyPool) -> Result<Vec<f64>, sqlx::Error> {
    sqlx::query("DROP TABLE IF EXISTS h").execute(pool).await?;
    sqlx::query("CREATE TABLE h (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO h SELECT i, (i % 100)::int, \
         ((i::bigint * 2654435761) % 100000)::int FROM generate_series(1, {N}) i"
    ))
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX h_v_idx ON h (v)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX h_g_idx ON h (g)")
        .execute(pool)
        .await?;
    sqlx::query("ANALYZE h").execute(pool).await?;

    let mut out = Vec::with_capacity(shapes().len());
    for (_, sql) in &shapes() {
        for _ in 0..WARMUP {
            sqlx::query("BEGIN").execute(pool).await?;
            sqlx::query(sql).execute(pool).await?;
            sqlx::query("ROLLBACK").execute(pool).await?;
        }
        let mut samples = Vec::with_capacity(runs());
        for _ in 0..runs() {
            sqlx::query("BEGIN").execute(pool).await?;
            let t0 = Instant::now();
            sqlx::query(sql).execute(pool).await?;
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
            sqlx::query("ROLLBACK").execute(pool).await?;
            // Rolled-back UPDATE/DELETE/INSERT still leaves dead
            // tuples — clean them (untimed) so later runs are honest.
            sqlx::query("VACUUM h").execute(pool).await?;
        }
        out.push(median_ms(samples));
    }
    Ok(out)
}
