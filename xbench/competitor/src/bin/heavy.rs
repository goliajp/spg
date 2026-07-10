//! Heavy read-shape latency — SPG (embedded) vs PostgreSQL 18.
//!
//! Phase-0 loss-hunt (perf-decomposition methodology): the `latency` bench
//! covers only single-row INSERT + PK SELECT, where SPGS already beats PG18.
//! To find a LOSING endpoint we need heavier shapes. This measures aggregate /
//! GROUP BY / range / ORDER-BY-LIMIT / DISTINCT over a 50k-row table.
//!
//! SPGE is the fastest SPG path (SPGE << SPGS); the SPGS wire adds only ~15µs
//! (measured in `latency`), which is noise for ms-scale heavy queries. So a
//! shape where SPGE loses to PG is a confirmed SPGS loss; a comfortable SPGE
//! win implies an SPGS win too.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin heavy`
//! Needs PostgreSQL reachable at the `connection_strings()` postgres URL.

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
use std::time::Instant;

const N: i64 = 50_000;
const WARMUP: usize = 3;
const RUNS: usize = 31; // odd → clean median

/// The read shapes under test. Same SQL text runs on both engines.
// avg(...) is cast to float8 — sqlx's Any driver can't decode PG NUMERIC, and
// the cast is a trivial, identical cost on both engines.
const SHAPES: &[(&str, &str)] = &[
    ("full_agg", "SELECT count(*), sum(v), avg(v)::float8 FROM h"),
    (
        "group_by",
        "SELECT g, count(*), sum(v) FROM h GROUP BY g ORDER BY g",
    ),
    (
        "range_count",
        "SELECT count(*) FROM h WHERE v BETWEEN 20000 AND 40000",
    ),
    (
        "order_limit",
        "SELECT id, v FROM h ORDER BY v DESC LIMIT 10",
    ),
    ("distinct_g", "SELECT count(DISTINCT g) FROM h"),
    (
        "filter_agg",
        "SELECT g, avg(v)::float8 FROM h WHERE v > 50000 GROUP BY g ORDER BY g",
    ),
];

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn median_ms(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    // SPGE (embedded, in-process).
    let spge = bench_spg_embedded();

    // PG18 leg.
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

    // Report.
    println!();
    println!("# heavy read shapes — median ms over {RUNS} runs, {N}-row table");
    println!("| shape        |   SPGE ms |   PG18 ms | SPGE/PG | verdict |");
    println!("|--------------|----------:|----------:|--------:|---------|");
    for (i, (name, _)) in SHAPES.iter().enumerate() {
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
            "| {:<12} | {:>9.3} | {:>9.3} | {:>6.2}× | {:<7} |",
            name, s, p, ratio, verdict
        );
    }
    println!();
    println!("# SPGS ≈ SPGE + ~15µs wire (noise at ms scale). SPGE loss ⇒ SPGS loss.");
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
    SHAPES
        .iter()
        .map(|(_, sql)| {
            for _ in 0..WARMUP {
                eng.execute(sql).unwrap();
            }
            let mut samples = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let t0 = Instant::now();
                eng.execute(sql).unwrap();
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
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
    // Fast bulk seed via generate_series (same formula as val_for).
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

    let mut out = Vec::with_capacity(SHAPES.len());
    for (_, sql) in SHAPES {
        for _ in 0..WARMUP {
            let _ = sqlx::query(sql).fetch_all(pool).await?;
        }
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t0 = Instant::now();
            let _ = sqlx::query(sql).fetch_all(pool).await?;
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        out.push(median_ms(samples));
    }
    Ok(out)
}
