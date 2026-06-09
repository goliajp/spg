//! v7.18 — sqlx Pool concurrent reads benchmark.
//!
//! Matrix (3 backends × 4 concurrencies × 3 workloads):
//!
//!   Backend                                  | Concurrency knob
//!   SpgPool (routing)                        | max_connections = 1, 4, 16, 64
//!   SpgPool + AsyncReadHandle (escape hatch) | N read_handles, one per task
//!   PgPool (pgvector container)              | max_connections = 1, 4, 16, 64
//!
//!   Workload     | Description
//!   pk-select    | SELECT WHERE id = $1 — single-row
//!   range-scan   | SELECT WHERE v BETWEEN $1 AND $2 — ~50 rows
//!   mixed-9to1   | 90% SELECT + 10% UPDATE, concurrent
//!
//! Metrics: queries/sec throughput, p50/p99/p999 latency (us).
//! Output: GitHub-flavoured markdown tables on stdout.
//!
//! PG container @ 127.0.0.1:25432 is OPTIONAL — if it's not
//! reachable, the harness fills PgPool rows with "[unavailable]"
//! so the SpgPool side still produces numbers in CI.
//!
//! Usage:
//!   cargo run --release -p spg-bench-competitor \
//!       --bin sqlx_pool_concurrent_reads -- [--iters N] [--rows N]

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::redundant_closure_for_method_calls
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use spg_embedded_tokio::AsyncDatabase;
use spg_sqlx::{SpgConnectOptions, SpgPool, SpgPoolOptions};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

const DEFAULT_ITERS: usize = 2_000;
const DEFAULT_ROWS: i32 = 10_000;
const PG_URL: &str = "postgres://bench:bench@127.0.0.1:25432/bench";
const CONCURRENCIES: &[usize] = &[1, 4, 16, 64];

#[derive(Debug, Clone)]
struct Stats {
    backend: String,
    concurrency: usize,
    workload: &'static str,
    available: bool,
    elapsed_ms: u128,
    throughput_qps: f64,
    p50_us: u64,
    p99_us: u64,
    p999_us: u64,
}

impl Stats {
    fn unavailable(backend: &str, concurrency: usize, workload: &'static str) -> Self {
        Self {
            backend: backend.into(),
            concurrency,
            workload,
            available: false,
            elapsed_ms: 0,
            throughput_qps: 0.0,
            p50_us: 0,
            p99_us: 0,
            p999_us: 0,
        }
    }
}

fn parse_args() -> (usize, i32) {
    let mut iters = DEFAULT_ITERS;
    let mut rows = DEFAULT_ROWS;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--iters" => {
                if let Some(v) = args.next().and_then(|s| s.parse::<usize>().ok()) {
                    iters = v;
                }
            }
            "--rows" => {
                if let Some(v) = args.next().and_then(|s| s.parse::<i32>().ok()) {
                    rows = v;
                }
            }
            _ => {}
        }
    }
    (iters, rows)
}

fn percentile(samples: &mut [u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() as f64 - 1.0) * p / 100.0).round() as usize;
    samples[idx.min(samples.len() - 1)]
}

fn summary(backend: &str, concurrency: usize, workload: &'static str, mut lats: Vec<u64>, elapsed: Duration) -> Stats {
    let n = lats.len() as f64;
    let elapsed_s = elapsed.as_secs_f64().max(1e-9);
    Stats {
        backend: backend.into(),
        concurrency,
        workload,
        available: true,
        elapsed_ms: elapsed.as_millis(),
        throughput_qps: n / elapsed_s,
        p50_us: percentile(&mut lats, 50.0),
        p99_us: percentile(&mut lats, 99.0),
        p999_us: percentile(&mut lats, 99.9),
    }
}

// -----------------------------------------------------------------
// SpgPool runners (sqlx Pool — routing on, fan-out internal)
// -----------------------------------------------------------------

async fn seed_spg(pool: &SpgPool, rows: i32) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("CREATE TABLE bench (id INT NOT NULL, v INT NOT NULL, label TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX bench_id_idx ON bench (id)")
        .execute(pool)
        .await?;
    for i in 0..rows {
        sqlx::query("INSERT INTO bench VALUES ($1, $2, $3)")
            .bind(i)
            .bind(i * 7)
            .bind(format!("row-{i}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn spg_pool_pk_select(pool: &SpgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let id = (i as i32) % rows;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            let row = sqlx::query("SELECT label FROM bench WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("select");
            let _: String = row.get(0);
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("SpgPool", concur, "pk-select", lats, t0.elapsed())
}

async fn spg_pool_range_scan(pool: &SpgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let lo = ((i as i32) % (rows - 50)) * 7;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            let _rows = sqlx::query("SELECT id FROM bench WHERE v BETWEEN $1 AND $2")
                .bind(lo)
                .bind(lo + 50 * 7)
                .fetch_all(&pool)
                .await
                .expect("range scan");
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("SpgPool", concur, "range-scan", lats, t0.elapsed())
}

async fn spg_pool_mixed(pool: &SpgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let id = (i as i32) % rows;
        let write = i % 10 == 0;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            if write {
                sqlx::query("UPDATE bench SET v = v + 1 WHERE id = $1")
                    .bind(id)
                    .execute(&pool)
                    .await
                    .expect("update");
            } else {
                let row = sqlx::query("SELECT v FROM bench WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("select");
                let _: i32 = row.get(0);
            }
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("SpgPool", concur, "mixed-9to1", lats, t0.elapsed())
}

// -----------------------------------------------------------------
// AsyncReadHandle bare fan-out (escape-hatch baseline)
// -----------------------------------------------------------------

async fn read_handle_pk_select(db: &AsyncDatabase, iters: usize, rows: i32, concur: usize) -> Stats {
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let db = db.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let id = (i as i32) % rows;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            let h = db.read_handle().await;
            let _rows = h
                .query(&format!("SELECT label FROM bench WHERE id = {id}"))
                .await
                .expect("query");
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("read_handle", concur, "pk-select", lats, t0.elapsed())
}

// -----------------------------------------------------------------
// PgPool runners
// -----------------------------------------------------------------

async fn ping_pg() -> Result<(), Box<dyn std::error::Error>> {
    let _ = TcpStream::connect("127.0.0.1:25432").await?;
    Ok(())
}

/// Read PG's `max_connections` GUC. The bench scales concurrency
/// up to this cap minus a small overhead reserve (1 for the
/// seeder, 1 for psql/admin). Used to skip PgPool concurrency
/// rows the server can't serve without rejecting connections.
async fn pg_max_connections(pool: &PgPool) -> u32 {
    sqlx::query("SHOW max_connections")
        .fetch_one(pool)
        .await
        .ok()
        .and_then(|row| row.get::<String, _>(0).parse::<u32>().ok())
        .unwrap_or(100)
}

async fn seed_pg(pool: &PgPool, rows: i32) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("DROP TABLE IF EXISTS bench")
        .execute(pool)
        .await?;
    sqlx::query("CREATE TABLE bench (id INT NOT NULL, v INT NOT NULL, label TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX bench_id_idx ON bench (id)")
        .execute(pool)
        .await?;
    for i in 0..rows {
        sqlx::query("INSERT INTO bench VALUES ($1, $2, $3)")
            .bind(i)
            .bind(i * 7)
            .bind(format!("row-{i}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn pg_pool_pk_select(pool: &PgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let id = (i as i32) % rows;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            let row = sqlx::query("SELECT label FROM bench WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("select");
            let _: String = row.get(0);
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("PgPool", concur, "pk-select", lats, t0.elapsed())
}

async fn pg_pool_range_scan(pool: &PgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let lo = ((i as i32) % (rows - 50)) * 7;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            let _rows = sqlx::query("SELECT id FROM bench WHERE v BETWEEN $1 AND $2")
                .bind(lo)
                .bind(lo + 50 * 7)
                .fetch_all(&pool)
                .await
                .expect("range scan");
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("PgPool", concur, "range-scan", lats, t0.elapsed())
}

async fn pg_pool_mixed(pool: &PgPool, iters: usize, rows: i32, concur: usize) -> Stats {
    let pool = pool.clone();
    let sem = Arc::new(Semaphore::new(concur));
    let lats = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(iters)));
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(iters);
    for i in 0..iters {
        let pool = pool.clone();
        let sem = sem.clone();
        let lats = lats.clone();
        let id = (i as i32) % rows;
        let write = i % 10 == 0;
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let q = Instant::now();
            if write {
                sqlx::query("UPDATE bench SET v = v + 1 WHERE id = $1")
                    .bind(id)
                    .execute(&pool)
                    .await
                    .expect("update");
            } else {
                let row = sqlx::query("SELECT v FROM bench WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("select");
                let _: i32 = row.get(0);
            }
            let us = q.elapsed().as_micros() as u64;
            lats.lock().await.push(us);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let lats = Arc::try_unwrap(lats).unwrap().into_inner();
    summary("PgPool", concur, "mixed-9to1", lats, t0.elapsed())
}

// -----------------------------------------------------------------
// Markdown output
// -----------------------------------------------------------------

fn print_markdown_table(results: &[Stats]) {
    // Sort: backend → workload → concurrency for readability.
    let mut sorted: Vec<Stats> = results.to_vec();
    sorted.sort_by(|a, b| {
        a.backend
            .cmp(&b.backend)
            .then(a.workload.cmp(b.workload))
            .then(a.concurrency.cmp(&b.concurrency))
    });
    println!("| Backend | Workload | Concurrency | Throughput (q/s) | p50 (us) | p99 (us) | p999 (us) | Elapsed (ms) |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    for s in &sorted {
        if s.available {
            println!(
                "| {} | {} | {} | {:.0} | {} | {} | {} | {} |",
                s.backend,
                s.workload,
                s.concurrency,
                s.throughput_qps,
                s.p50_us,
                s.p99_us,
                s.p999_us,
                s.elapsed_ms,
            );
        } else {
            println!(
                "| {} | {} | {} | [unavailable] | — | — | — | — |",
                s.backend, s.workload, s.concurrency,
            );
        }
    }
}

// -----------------------------------------------------------------
// main
// -----------------------------------------------------------------

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (iters, rows) = parse_args();
    eprintln!("=== sqlx Pool concurrent reads bench (v7.18) ===");
    eprintln!("iters={iters} rows={rows} concurrencies={CONCURRENCIES:?}");

    let mut results: Vec<Stats> = Vec::new();

    // --- 1. SpgPool (routing on, file-backed) ---
    //
    // One `SpgConnectOptions` is shared across every Pool we
    // construct in this run: the inner `Arc<OnceCell<AsyncDatabase>>`
    // makes every `connect()` resolve to the SAME underlying engine,
    // so the file lock only fires once. SpgPool::close releases
    // sqlx-side connection slots but leaves the engine alive in
    // the OnceCell, so successive Pool constructions at different
    // max_connections don't race the catalog file.
    let tmp = TempDir::new()?;
    let catalog_path = tmp.path().join("bench.spg");
    let opts = SpgConnectOptions::file(catalog_path.clone());
    eprintln!(
        "\n[setup] SpgPool — seeding {rows} rows into {}",
        catalog_path.display()
    );
    {
        let seed_pool = SpgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await?;
        seed_spg(&seed_pool, rows).await?;
        seed_pool.close().await;
    }
    for &concur in CONCURRENCIES {
        eprintln!("[run] SpgPool max_connections={concur}");
        let pool = SpgPoolOptions::new()
            .max_connections(concur as u32)
            .connect_with(opts.clone())
            .await?;
        results.push(spg_pool_pk_select(&pool, iters, rows, concur).await);
        results.push(spg_pool_range_scan(&pool, iters, rows, concur).await);
        results.push(spg_pool_mixed(&pool, iters, rows, concur).await);
        pool.close().await;
    }

    // --- 2. read_handle bare (escape-hatch baseline) ---
    //
    // Reuse the OnceCell-shared AsyncDatabase by acquiring a
    // connection off a 1-slot SpgPool and cloning its engine
    // handle. No second `open_path` → no file lock race with the
    // engine still cached by `opts.shared`.
    eprintln!("\n[setup] read_handle bare — reusing shared AsyncDatabase from opts");
    let db = {
        let pool = SpgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await?;
        let conn = pool.acquire().await?;
        let db: AsyncDatabase = conn.engine().clone();
        drop(conn);
        pool.close().await;
        db
    };
    for &concur in CONCURRENCIES {
        eprintln!("[run] read_handle concurrency={concur}");
        results.push(read_handle_pk_select(&db, iters, rows, concur).await);
    }

    // --- 3. PgPool (pgvector container, optional) ---
    eprintln!("\n[setup] PgPool — probing {PG_URL}");
    if ping_pg().await.is_ok() {
        eprintln!("[setup] PG reachable — seeding {rows} rows");
        let seed_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(PG_URL)
            .await?;
        let pg_cap = pg_max_connections(&seed_pool).await.saturating_sub(2);
        eprintln!("[setup] PG max_connections cap = {pg_cap} (after admin reserve)");
        seed_pg(&seed_pool, rows).await?;
        seed_pool.close().await;
        for &concur in CONCURRENCIES {
            if (concur as u32) > pg_cap {
                eprintln!(
                    "[skip] PgPool max_connections={concur} exceeds server cap {pg_cap}; reporting [unavailable]"
                );
                for workload in ["pk-select", "range-scan", "mixed-9to1"] {
                    results.push(Stats::unavailable("PgPool", concur, workload));
                }
                continue;
            }
            eprintln!("[run] PgPool max_connections={concur}");
            let pool = PgPoolOptions::new()
                .max_connections(concur as u32)
                .acquire_timeout(Duration::from_secs(120))
                .connect(PG_URL)
                .await?;
            results.push(pg_pool_pk_select(&pool, iters, rows, concur).await);
            results.push(pg_pool_range_scan(&pool, iters, rows, concur).await);
            results.push(pg_pool_mixed(&pool, iters, rows, concur).await);
            pool.close().await;
        }
    } else {
        eprintln!("[skipped] PG container not reachable at {PG_URL}");
        eprintln!("         (run `docker compose -f xbench/competitor/docker-compose.yml up -d postgres` to enable)");
        for &concur in CONCURRENCIES {
            for workload in ["pk-select", "range-scan", "mixed-9to1"] {
                results.push(Stats::unavailable("PgPool", concur, workload));
            }
        }
    }

    println!("\n## sqlx Pool concurrent reads — v7.18 benchmark\n");
    println!("Workloads:");
    println!("- **pk-select** — `SELECT label FROM bench WHERE id = $1` (point lookup)");
    println!("- **range-scan** — `SELECT id FROM bench WHERE v BETWEEN $1 AND $2` (~50 rows)");
    println!("- **mixed-9to1** — 90% pk-select + 10% UPDATE\n");
    println!("Backends:");
    println!("- **SpgPool** — sqlx adapter, per-statement snapshot routing (v7.18)");
    println!("- **read_handle** — `AsyncReadHandle` escape-hatch (SPG-private, bypasses sqlx)");
    println!("- **PgPool** — `pgvector/pgvector:pg18` over the wire @ `127.0.0.1:25432`\n");
    print_markdown_table(&results);
    eprintln!("\n=== done ===");
    Ok(())
}
