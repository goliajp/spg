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
const N5: i64 = 500_000;
const WARMUP: usize = 3;
const RUNS: usize = 31; // odd → clean median

/// v7.39 (round 477) — effective run count, overridable without a rebuild.
///
/// At 31 runs the per-side medians are stable (SPGE 1-8 %, PG 4-5 % sample
/// stdev) but their RATIO carries about +/-10 %: `big_in` read 0.65x, 0.76x,
/// 0.83x and 0.87x across four back-to-back runs. The six shapes the audit
/// called "narrow wins" all sit inside 0.72-0.87, which is narrower than
/// that band — so the ledger's ORDERING of them was noise, and picking an
/// attack target off it would be picking noise. The methodology's answer to
/// a band too wide to rank is to widen the sample, not to reason harder
/// about the numbers.
fn runs() -> usize {
    std::env::var("SPG_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 3)
        .unwrap_or(RUNS)
}

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
    // ---- extended loss-hunt shapes (v7.37.16): join / subquery /
    // sort-page / big IN / DISTINCT projection. `d` is a 100-row
    // dimension table (g -> label).
    (
        "join_agg",
        "SELECT d.label, count(*), sum(h.v) FROM h JOIN d ON h.g = d.g \
         GROUP BY d.label ORDER BY d.label",
    ),
    (
        "scalar_subq",
        "SELECT count(*) FROM h WHERE v > (SELECT avg(v) FROM h)",
    ),
    (
        "in_subq",
        "SELECT count(*) FROM h WHERE g IN (SELECT g FROM d WHERE g < 50)",
    ),
    // Forces a real sort of the full table (top-N heap can't shortcut a
    // mid-table OFFSET) while keeping the fetched page small so the PG
    // leg isn't dominated by 50k-row wire transfer.
    (
        "sort_page",
        "SELECT v FROM h ORDER BY v LIMIT 100 OFFSET 25000",
    ),
    (
        "big_in",
        "SELECT count(*) FROM h WHERE g IN (1,3,5,7,9,11,13,15,17,19,21,23,25,27,29,\
         31,33,35,37,39,41,43,45,47,49,51,53,55,57,59,61,63,65,67,69,71,73,75,77,79,\
         81,83,85,87,89,91,93,95,97,99)",
    ),
    ("distinct_proj", "SELECT DISTINCT g FROM h ORDER BY g"),
    // ---- loss-hunt round 3 (v7.37.16): TEXT-heavy / correlated
    // subquery / 500k scale. `ht` = 50k rows with a TEXT tag column
    // (1000 uniques); `h5` = 500k-row copy of the h shape.
    (
        "text_group",
        "SELECT s, count(*) FROM ht GROUP BY s ORDER BY s LIMIT 10",
    ),
    (
        "like_filter",
        "SELECT count(*) FROM ht WHERE s LIKE '%_05%'",
    ),
    ("text_distinct", "SELECT count(DISTINCT s) FROM ht"),
    (
        "corr_subq",
        "SELECT count(*) FROM d WHERE g < (SELECT avg(h.v) FROM h WHERE h.g = d.g)",
    ),
    (
        "agg_500k",
        "SELECT count(*), sum(v), avg(v)::float8 FROM h5",
    ),
    (
        "range_500k",
        "SELECT count(*) FROM h5 WHERE v BETWEEN 20000 AND 40000",
    ),
    (
        "group_500k",
        "SELECT g, count(*), sum(v) FROM h5 GROUP BY g ORDER BY g",
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
    println!(
        "# heavy read shapes — median ms over {} runs, {N}-row table",
        runs()
    );
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
    // v7.39 (parallel-agg) — a real embedded customer goes through
    // spg_embedded::Database, which injects the std parallel runner;
    // the bench builds a bare Engine, so mirror that injection here
    // (same SPG_PARALLEL opt-out) or the panel silently measures the
    // serial path forever.
    if !std::env::var("SPG_PARALLEL")
        .is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
    {
        struct R;
        impl spg_engine::ParallelRunner for R {
            fn run_shards(
                &self,
                n: usize,
                f: &(dyn Fn(usize) -> Box<dyn core::any::Any + Send> + Sync),
            ) -> Vec<Box<dyn core::any::Any + Send>> {
                std::thread::scope(|s| {
                    let hs: Vec<_> = (0..n).map(|i| s.spawn(move || f(i))).collect();
                    hs.into_iter().map(|h| h.join().unwrap()).collect()
                })
            }
        }
        eng.set_parallel_runner(std::sync::Arc::new(R));
    }
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    eng.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    for i in 1..=N {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h VALUES ({i}, {g}, {v})"))
            .unwrap();
    }
    eng.execute("CREATE TABLE d (g INT NOT NULL, label TEXT NOT NULL)")
        .unwrap();
    for g in 0..100 {
        eng.execute(&format!("INSERT INTO d VALUES ({g}, 'grp_{g:03}')"))
            .unwrap();
    }
    eng.execute("CREATE TABLE ht (id INT NOT NULL, s TEXT NOT NULL)")
        .unwrap();
    for i in 1..=N {
        let tag = i % 1000;
        eng.execute(&format!("INSERT INTO ht VALUES ({i}, 'user_{tag:04}')"))
            .unwrap();
    }
    eng.execute("CREATE TABLE h5 (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX h5_v_idx ON h5 (v)").unwrap();
    for i in 1..=N5 {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h5 VALUES ({i}, {g}, {v})"))
            .unwrap();
    }
    SHAPES
        .iter()
        .map(|(_, sql)| {
            for _ in 0..WARMUP {
                eng.execute(sql).unwrap();
            }
            let mut samples = Vec::with_capacity(runs());
            for _ in 0..runs() {
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
    sqlx::query("DROP TABLE IF EXISTS d").execute(pool).await?;
    sqlx::query("CREATE TABLE d (g INT NOT NULL, label TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO d SELECT g, 'grp_' || lpad(g::text, 3, '0') \
         FROM generate_series(0, 99) g",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TABLE IF EXISTS ht").execute(pool).await?;
    sqlx::query("CREATE TABLE ht (id INT NOT NULL, s TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO ht SELECT i, 'user_' || lpad((i % 1000)::text, 4, '0') \
         FROM generate_series(1, {N}) i"
    ))
    .execute(pool)
    .await?;
    sqlx::query("DROP TABLE IF EXISTS h5").execute(pool).await?;
    sqlx::query("CREATE TABLE h5 (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO h5 SELECT i, (i % 100)::int, \
         ((i::bigint * 2654435761) % 100000)::int FROM generate_series(1, {N5}) i"
    ))
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX h5_v_idx ON h5 (v)")
        .execute(pool)
        .await?;
    sqlx::query("ANALYZE h").execute(pool).await?;
    sqlx::query("ANALYZE d").execute(pool).await?;
    sqlx::query("ANALYZE ht").execute(pool).await?;
    sqlx::query("ANALYZE h5").execute(pool).await?;

    let mut out = Vec::with_capacity(SHAPES.len());
    for (_, sql) in SHAPES {
        for _ in 0..WARMUP {
            let _ = sqlx::query(sql).fetch_all(pool).await?;
        }
        let mut samples = Vec::with_capacity(runs());
        for _ in 0..runs() {
            let t0 = Instant::now();
            let _ = sqlx::query(sql).fetch_all(pool).await?;
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        out.push(median_ms(samples));
    }
    Ok(out)
}
