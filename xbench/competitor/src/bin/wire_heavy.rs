//! r175 — SPGS (spg-server over **pgwire**) vs PostgreSQL 18, write-heavy
//! shapes, both commit modes.
//!
//! The vision red line is SPGS > PG18 on the SERVER wire path; the r164/r174
//! panels proved the embedded (SPGE) engine but never the wire. This panel
//! drives BOTH engines through the exact same client stack — sqlx's
//! postgres protocol driver over TCP — so the comparison includes protocol
//! encode/decode, per-statement round trips and each server's own write
//! path (SPGS: pgwire persist_wire_write + WAL; PG: its normal backend).
//!
//! SPGS is spawned fresh (release binary, durable db + WAL args,
//! `SPG_PG_ADDR=127.0.0.1:0`) and its kernel-assigned pgwire port parsed
//! from stderr. `SET synchronous_commit = off` is issued as plain customer
//! SQL on both sides (real on SPGS since r172).
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin wire_heavy`
//! (builds spg-server first; needs the bench PG container up).

#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

use spg_bench_competitor::connection_strings;
use spg_bench_competitor::write_shapes::{N, RUNS, SHAPES, bench_engine, verdict};
use sqlx::any::AnyPoolOptions;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Spawn a fresh durable spg-server with a pgwire listener; return the
/// child + the pgwire addr parsed from its stderr "pg-wire listening
/// on" line.
fn spawn_spgs(dir: &std::path::Path) -> Result<(Child, String), Box<dyn std::error::Error>> {
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let mut child = Command::new(&bin)
        .arg("127.0.0.1:0")
        .arg(dir.join("bench.spgdb"))
        .arg("-")
        .arg(dir.join("bench.wal"))
        .env("SPG_PG_ADDR", "127.0.0.1:0")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let start = Instant::now();
    let mut line = String::new();
    while start.elapsed() < Duration::from_secs(10) {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if let Some(rest) = line.trim_end().strip_prefix("spg-server: pg-wire listening on ") {
            let addr = rest.to_string();
            // Keep draining stderr so the server never blocks on a full
            // pipe once real traffic starts logging.
            std::thread::spawn(move || {
                let mut sink = String::new();
                while let Ok(n) = reader.read_line(&mut sink) {
                    if n == 0 {
                        break;
                    }
                    sink.clear();
                }
            });
            return Ok((child, addr));
        }
    }
    let _ = child.kill();
    Err(format!("spg-server pgwire didn't report ready; last line: {line}").into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let rt = tokio::runtime::Handle::current();

    // Generic: run the suite over any postgres-protocol URL with a
    // given synchronous_commit mode, on a single connection.
    let bench_url = |url: String, mode: &'static str| -> Result<Vec<f64>, String> {
        let rt = rt.clone();
        tokio::task::block_in_place(move || {
            let pool = rt
                .block_on(
                    AnyPoolOptions::new()
                        .max_connections(1)
                        .acquire_timeout(Duration::from_secs(10))
                        .connect(&url),
                )
                .map_err(|e| format!("connect {url}: {e}"))?;
            rt.block_on(async {
                sqlx::query("DROP TABLE IF EXISTS wb")
                    .execute(&pool)
                    .await
                    .map_err(|e| format!("drop: {e}"))?;
                sqlx::query(&format!("SET synchronous_commit = {mode}"))
                    .execute(&pool)
                    .await
                    .map_err(|e| format!("set sync: {e}"))?;
                Ok::<(), String>(())
            })?;
            let rt2 = rt.clone();
            let pool2 = pool.clone();
            let out = bench_engine(&mut |sql| {
                rt2.block_on(async {
                    sqlx::query(sql).execute(&pool2).await.unwrap();
                });
            });
            rt.block_on(async {
                let _ = sqlx::query("DROP TABLE IF EXISTS wb").execute(&pool).await;
                pool.close().await;
            });
            Ok(out)
        })
    };

    // ---- SPGS over pgwire, fresh durable server per mode ----
    //
    // r195 (docker-fair) — `SPGS_URL` overrides the local spawn: point
    // it at a containerized SPGS so both engines pay the same
    // container-filesystem fsync cost. The caller owns the container's
    // lifecycle and state reset between runs.
    let bench_spgs = |mode: &'static str| -> Result<Vec<f64>, Box<dyn std::error::Error>> {
        if let Ok(url) = std::env::var("SPGS_URL") {
            return bench_url(url, mode).map_err(Into::into);
        }
        let dir = std::env::temp_dir().join(format!(
            "spgs-wire-heavy-{mode}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        // v7.39 (round 440) — docker-fair. Spawning SPGS natively while PG18
        // runs in a Linux container compares two DURABILITY CONTRACTS, not two
        // implementations: on macOS `sync_data` is F_FULLFSYNC (a real device
        // flush, measured at ~4 ms by `fsync_probe`) while a container's
        // `fdatasync` hits a virtual disk and never becomes one (~0.06 ms for
        // the host's plain fsync). `SPG_WIRE_URL` points this panel at an
        // ALREADY-RUNNING SPGS — put it in a Linux container beside PG and
        // both legs finally get the same contract.
        if let Ok(url) = std::env::var("SPG_WIRE_URL")
            && !url.is_empty()
        {
            return bench_url(url, mode).map_err(std::convert::Into::into);
        }
        std::fs::create_dir_all(&dir)?;
        let (mut child, addr) = spawn_spgs(&dir)?;
        let url = format!("postgres://bench:bench@{addr}/bench");
        let out = bench_url(url, mode).map_err(|e| -> Box<dyn std::error::Error> {
            let _ = child.kill();
            e.into()
        })?;
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        Ok(out)
    };
    let spgs_on = bench_spgs("on")?;
    let spgs_off = bench_spgs("off")?;

    // ---- PostgreSQL 18, same client stack ----
    let pg_url = connection_strings()
        .into_iter()
        .find(|(n, _)| *n == "postgres")
        .map(|(_, u)| u)
        .ok_or("no postgres connection string")?;
    let pg_on = bench_url(pg_url.clone(), "on")?;
    let pg_off = bench_url(pg_url, "off")?;

    println!("# wire write-heavy shapes — median ms over {RUNS} runs, {N}-row seeded table");
    println!("# SPGS = spg-server DURABLE (db+WAL) via pgwire; PG18 = postgres:18-alpine");
    println!("# identical client stack (sqlx postgres driver, 1 conn); same SQL both sides");
    println!(
        "| shape             | SPGSon ms |  PGon ms |  on-ratio | SPGSoff ms | PGoff ms | off-ratio |"
    );
    println!(
        "|-------------------|----------:|---------:|----------:|-----------:|---------:|----------:|"
    );
    for (i, (name, _)) in SHAPES.iter().enumerate() {
        let r_on = spgs_on[i] / pg_on[i];
        let r_off = spgs_off[i] / pg_off[i];
        println!(
            "| {:<17} | {:>9.3} | {:>8.3} | {:>4.2}× {:<5} | {:>10.3} | {:>8.3} | {:>4.2}× {:<5} |",
            name,
            spgs_on[i],
            pg_on[i],
            r_on,
            verdict(r_on),
            spgs_off[i],
            pg_off[i],
            r_off,
            verdict(r_off)
        );
    }
    Ok(())
}
