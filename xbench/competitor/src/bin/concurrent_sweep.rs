//! v4.42 — multi-client INSERT throughput sweep across backends.
//!
//! For each backend (spg-server, postgres, mysql, mariadb), run a
//! grid of `(N concurrent writers) × (M single-row INSERTs per
//! writer)` and report the aggregate r/s. This is the workload
//! v4.42's group-commit unlock targets: N writers each pushing
//! auto-commit INSERTs through their own TCP connection, the
//! server-side commit-barrier leader coalescing them into groups
//! sharing a single fsync.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin
//!       concurrent_sweep`
//!
//! Notes:
//! - `SPG_COMMIT_DELAY_US` is set to 200 µs for the spg-server
//!   run — that's the spin window the leader gives concurrent
//!   writers to populate the queue before forming a group. On
//!   macOS APFS (fsync ~5-7 ms) the 200 µs is cheap insurance.
//! - The competitor stack expects the docker-compose containers
//!   to be healthy — see `xbench/competitor/scripts/up.sh`.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion
)]

use spg_bench_competitor::connection_strings;
use sqlx::AnyPool;
use sqlx::any::AnyPoolOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const SPG_SERVER_ADDR: &str = "127.0.0.1:25559";
const CLIENT_COUNTS: &[usize] = &[1, 4, 8];
const PER_THREAD: usize = 500;

#[derive(Debug, Clone)]
struct Row {
    backend: String,
    clients: usize,
    total_writes: usize,
    wall_sec: f64,
    aggregate_rps: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    println!("# v4.42 multi-client INSERT throughput sweep");
    println!();
    println!(
        "# {} writes/thread, threads = {:?}",
        PER_THREAD, CLIENT_COUNTS
    );
    println!();

    let mut rows: Vec<Row> = Vec::new();

    // --- spg-server ---
    eprintln!("==> spg-server");
    let mut child = spawn_spg_server()?;
    if let Err(e) = run_spg_server(&mut rows) {
        eprintln!("  spg-server bench errored: {e}");
    }
    let _ = child.kill();
    let _ = child.wait();

    // --- competitor backends via sqlx ---
    for (label, url) in connection_strings() {
        eprintln!("==> {label}");
        runtime.block_on(async {
            for &n in CLIENT_COUNTS {
                match run_sqlx_concurrent(label, &url, n).await {
                    Ok(row) => rows.push(row),
                    Err(e) => eprintln!("  {label}[c={n}] errored: {e}"),
                }
            }
        });
    }

    // --- print markdown table ---
    println!("| backend       | clients | writes | wall (s) | aggregate r/s |");
    println!("|---------------|--------:|-------:|---------:|--------------:|");
    for r in &rows {
        println!(
            "| {:<13} | {:>7} | {:>6} | {:>8.3} | {:>13.0} |",
            r.backend, r.clients, r.total_writes, r.wall_sec, r.aggregate_rps,
        );
    }

    Ok(())
}

// ---- spg-server ---------------------------------------------------

fn spawn_spg_server() -> Result<Child, Box<dyn std::error::Error>> {
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let tmp = std::env::temp_dir().join(format!("spg-csweep-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let db_path = tmp.join("a.db");
    let wal_path = tmp.join("a.wal");
    let mut child = Command::new(&bin)
        .arg(SPG_SERVER_ADDR)
        .arg(&db_path)
        .arg("-")
        .arg(&wal_path)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        // v7.38.22 — the window is NOT pinned here any more.
        //
        // This set `SPG_COMMIT_DELAY_US=200` with a comment saying it
        // made concurrent writers coalesce. An explicit value PINS the
        // leader's window and turns OFF the adaptive one that ships as
        // the default (`effective_commit_delay_us`), so this harness —
        // built to measure group commit — was measuring the mechanism in
        // its off position, and so was the SLO gate until earlier in this
        // version.
        //
        // Unset, the server adapts, which is what a deployment does. Set
        // it in the harness's OWN environment to pin it and compare:
        // `SPG_COMMIT_DELAY_US=0 cargo run … --bin concurrent_sweep`.
        .envs(
            std::env::var("SPG_COMMIT_DELAY_US")
                .ok()
                .map(|v| ("SPG_COMMIT_DELAY_US".to_string(), v)),
        )
        .spawn()?;
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let start = Instant::now();
    let mut line = String::new();
    while start.elapsed() < Duration::from_secs(5) {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.contains("listening on") {
            std::thread::spawn(move || {
                let mut sink = String::new();
                let _ = BufReader::new(reader).read_to_string(&mut sink);
            });
            return Ok(child);
        }
    }
    let _ = child.kill();
    Err("spg-server didn't print 'listening on' in 5s".into())
}

fn run_spg_server(rows: &mut Vec<Row>) -> Result<(), Box<dyn std::error::Error>> {
    // Create the table once via a setup connection.
    {
        let mut s = TcpStream::connect(SPG_SERVER_ADDR)?;
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        spg_exec_ok(
            &mut s,
            "CREATE TABLE csweep (tid INT NOT NULL, i INT NOT NULL)",
        )?;
    }

    for &n in CLIENT_COUNTS {
        eprintln!("  [{n} clients] streaming {PER_THREAD} INSERTs each");
        let started = Instant::now();
        let mut handles = Vec::with_capacity(n);
        let inserted = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(n + 1));
        // Offset writes per round so collisions across rounds are
        // visible if any; row count assertion picks them up.
        let round = rows.len();
        for t in 0..n {
            let inserted = Arc::clone(&inserted);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || -> Result<(), String> {
                let mut s = TcpStream::connect(SPG_SERVER_ADDR).map_err(|e| e.to_string())?;
                s.set_nodelay(true).ok();
                s.set_read_timeout(Some(Duration::from_mins(1)))
                    .map_err(|e| e.to_string())?;
                barrier.wait();
                for i in 0..PER_THREAD {
                    let tid = round * 100 + t;
                    spg_exec_ok(&mut s, &format!("INSERT INTO csweep VALUES ({tid}, {i})"))
                        .map_err(|e| e.to_string())?;
                }
                inserted.fetch_add(PER_THREAD, Ordering::Relaxed);
                Ok(())
            }));
        }
        barrier.wait();
        let bench_start = Instant::now();
        for h in handles {
            h.join()
                .map_err(|_| "worker panic")?
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        }
        let wall = bench_start.elapsed().as_secs_f64();
        let total = inserted.load(Ordering::Relaxed);
        let rps = total as f64 / wall;
        rows.push(Row {
            backend: "spg-server".into(),
            clients: n,
            total_writes: total,
            wall_sec: wall,
            aggregate_rps: rps,
        });
        eprintln!(
            "    {} writes in {:.3} s → {:.0} r/s (start→done {:.3}s incl. barrier)",
            total,
            wall,
            rps,
            started.elapsed().as_secs_f64(),
        );
    }
    Ok(())
}

fn spg_exec_ok(s: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    use spg_wire::{Op, build_query, decode, encode};
    let f = build_query(sql);
    let mut out = Vec::new();
    encode(&f, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    s.write_all(&out)?;
    let mut buf = Vec::with_capacity(64);
    let mut chunk = [0u8; 4096];
    loop {
        match decode(&buf) {
            Ok((frame, consumed)) => {
                buf.drain(..consumed);
                match frame.op {
                    Op::CommandComplete => return Ok(()),
                    Op::ErrorResponse | Op::Error => {
                        let msg = spg_wire::parse_error_response(&frame).unwrap_or("<undecodable>");
                        return Err(std::io::Error::other(format!(
                            "spg-server: {msg} (sql={sql:?})"
                        )));
                    }
                    _ => {}
                }
            }
            Err(spg_wire::FrameError::ShortHeader | spg_wire::FrameError::ShortPayload) => {
                let n = s.read(&mut chunk)?;
                if n == 0 {
                    return Err(std::io::Error::other("eof before CC"));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err(std::io::Error::other(e.to_string())),
        }
    }
}

// ---- competitor backends via sqlx ---------------------------------

async fn run_sqlx_concurrent(
    label: &str,
    url: &str,
    n: usize,
) -> Result<Row, Box<dyn std::error::Error>> {
    // One pool with N connections (one per writer).
    let pool: AnyPool = AnyPoolOptions::new()
        .max_connections(u32::try_from(n).unwrap_or(8))
        .acquire_timeout(Duration::from_secs(30))
        .connect(url)
        .await?;

    sqlx::query("DROP TABLE IF EXISTS csweep")
        .execute(&pool)
        .await
        .ok();
    sqlx::query("CREATE TABLE csweep (tid INT NOT NULL, i INT NOT NULL)")
        .execute(&pool)
        .await?;

    let bench_start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for t in 0..n {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER_THREAD {
                sqlx::query(&format!("INSERT INTO csweep VALUES ({t}, {i})"))
                    .execute(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(())
        }));
    }
    for h in handles {
        h.await??;
    }
    let wall = bench_start.elapsed().as_secs_f64();
    let total = n * PER_THREAD;
    let rps = total as f64 / wall;
    pool.close().await;
    Ok(Row {
        backend: label.into(),
        clients: n,
        total_writes: total,
        wall_sec: wall,
        aggregate_rps: rps,
    })
}
