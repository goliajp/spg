//! Write-heavy latency — SPG (embedded, DURABLE file backend) vs PostgreSQL 18.
//!
//! Phase-0 loss-hunt extension (r164): the read panel (`heavy`) shows 19/19
//! WINs, so the remaining loss frontier is writes. SPG runs
//! `spg_embedded::Database::open_path` — the real durable customer path (WAL
//! + fsync semantics), NOT the bare in-memory Engine the read panel uses.
//!
//! r174 — both engines now run BOTH commit modes, compared like-for-like:
//!
//!  * `sync=on` (each engine's durable default): the honest durability
//!    column — every commit waits for its fsync point.
//!  * `sync=off` (the identical customer SQL `SET synchronous_commit = off`
//!    on both engines — real on SPG since r171/r172): PG's documented
//!    latency trade, commit returns before flush.
//!
//! The r164 original compared SPG-durable against PG-off only (tilted
//! against SPG so a WIN was unarguable); with the session GUC real on SPG
//! the panel can finally show both fair pairings.
//!
//! r175 — the shape suite moved to `spg_bench_competitor::write_shapes`,
//! shared with the SPGS-over-pgwire panel (`wire_heavy`).
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin write_heavy`
//! Needs PostgreSQL reachable at the `connection_strings()` postgres URL.

#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

use spg_bench_competitor::connection_strings;
use spg_bench_competitor::write_shapes::{N, RUNS, SHAPES, bench_engine, verdict};
use sqlx::any::AnyPoolOptions;

/// One full SPG pass on a fresh durable database. `sync_off` issues the
/// same customer SQL PG gets — the r171 session GUC, not an env knob.
fn bench_spg(sync_off: bool) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!(
        "spg-write-heavy-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let mut db = spg_embedded::Database::open_path(dir.join("bench.db"))
        .map_err(|e| format!("open_path: {e}"))?;
    if sync_off {
        db.execute("SET synchronous_commit = off")
            .map_err(|e| format!("SET synchronous_commit: {e}"))?;
    }
    let out = bench_engine(&mut |sql| {
        db.execute(sql).unwrap();
    });
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    // ---- SPG embedded, durable file backend, both commit modes ----
    let spg_on = bench_spg(false)?;
    let spg_off = bench_spg(true)?;

    // ---- PostgreSQL 18, both commit modes ----
    let pg_url = connection_strings()
        .into_iter()
        .find(|(n, _)| *n == "postgres")
        .map(|(_, u)| u)
        .ok_or("no postgres connection string")?;
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&pg_url)
        .await?;
    let rt = tokio::runtime::Handle::current();
    let bench_pg = |mode: &'static str| {
        let pool = pool.clone();
        let rt = rt.clone();
        tokio::task::block_in_place(move || {
            rt.block_on(async {
                sqlx::query("DROP TABLE IF EXISTS wb")
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query(&format!("SET synchronous_commit = {mode}"))
                    .execute(&pool)
                    .await
                    .unwrap();
            });
            let rt2 = rt.clone();
            let pool2 = pool.clone();
            let out = bench_engine(&mut |sql| {
                rt2.block_on(async {
                    sqlx::query(sql).execute(&pool2).await.unwrap();
                });
            });
            rt.block_on(async {
                sqlx::query("DROP TABLE IF EXISTS wb")
                    .execute(&pool)
                    .await
                    .unwrap();
            });
            out
        })
    };
    let pg_on = bench_pg("on");
    let pg_off = bench_pg("off");

    println!("# write-heavy shapes — median ms over {RUNS} runs, {N}-row seeded table");
    println!("# SPG = spg-embedded DURABLE (open_path); PG18 = postgres:18-alpine");
    println!("# like-for-like: sync=on both sides | sync=off both sides (same customer SQL)");
    println!(
        "| shape             | SPGon ms |  PGon ms |  on-ratio | SPGoff ms | PGoff ms | off-ratio |"
    );
    println!(
        "|-------------------|---------:|---------:|----------:|----------:|---------:|----------:|"
    );
    for (i, (name, _)) in SHAPES.iter().enumerate() {
        let r_on = spg_on[i] / pg_on[i];
        let r_off = spg_off[i] / pg_off[i];
        println!(
            "| {:<17} | {:>8.3} | {:>8.3} | {:>4.2}× {:<5} | {:>9.3} | {:>8.3} | {:>4.2}× {:<5} |",
            name,
            spg_on[i],
            pg_on[i],
            r_on,
            verdict(r_on),
            spg_off[i],
            pg_off[i],
            r_off,
            verdict(r_off)
        );
    }
    Ok(())
}
