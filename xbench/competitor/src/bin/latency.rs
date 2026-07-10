//! Latency bench — single-row INSERT and single-row SELECT (point
//! lookup on PK) across SPG (embedded), PostgreSQL, MySQL, MariaDB.
//!
//! Run:  cargo run --release -p spg-bench-competitor --bin latency
//!
//! Each backend gets the same seed schema (`bench_users(id INT PK,
//! name TEXT)`), the same warm-up, and the same `ITERS` measured
//! samples. Output: p50 / p95 / p99 per (backend, op), in microseconds.

// Bench-code allow-list — same spirit as benches/*.rs in the main
// stones: dev-only code, perf numbers are the point, not lint
// ceremony.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::useless_conversion
)]

use spg_bench_competitor::connection_strings;
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, Row};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SEED_ROWS: i32 = 1000;
const WARMUP: usize = 200;
const ITERS: usize = 2000;
const SPG_SERVER_ADDR: &str = "127.0.0.1:25544";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    let mut rows: Vec<Row3> = Vec::new();

    // SPG embedded — in-process Engine, no wire.
    let (insert, select) = bench_spg_embedded();
    rows.push(Row3 {
        backend: "spg-embedded".into(),
        insert,
        select,
    });

    // SPG via TCP wire — spawn the server binary, run the same shape
    // of bench through spg-wire frames, kill the server.
    {
        let mut child = spawn_spg_server()?;
        let (insert, select) = bench_spg_server()?;
        let _ = child.kill();
        let _ = child.wait();
        rows.push(Row3 {
            backend: "spg-server".into(),
            insert,
            select,
        });
    }

    // Three TCP-fronted competitor servers via sqlx. A backend that isn't up
    // is skipped (logged to stderr) rather than aborting the whole sweep — the
    // SPG-vs-PG18 comparison must still print when MySQL/MariaDB are absent.
    for (label, url) in connection_strings() {
        let pool = match AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip {label}: connect failed ({e})");
                continue;
            }
        };
        match bench_via_sqlx(&pool, label).await {
            Ok((insert, select)) => rows.push(Row3 {
                backend: label.into(),
                insert,
                select,
            }),
            Err(e) => eprintln!("skip {label}: bench failed ({e})"),
        }
        pool.close().await;
    }

    print_table(&rows);
    Ok(())
}

struct Row3 {
    backend: String,
    insert: Stats,
    select: Stats,
}

#[derive(Default, Clone, Copy)]
struct Stats {
    p50: f64,
    p95: f64,
    p99: f64,
}

fn pct(samples: &mut [u64], p: f64) -> f64 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * p / 100.0).clamp(0.0, samples.len() as f64 - 1.0);
    samples[idx as usize] as f64 / 1000.0 // µs
}

fn stats_from(mut samples: Vec<u64>) -> Stats {
    Stats {
        p50: pct(&mut samples, 50.0),
        p95: pct(&mut samples, 95.0),
        p99: pct(&mut samples, 99.0),
    }
}

fn print_table(rows: &[Row3]) {
    println!();
    println!("# latency (µs) — single-row INSERT + single-row SELECT WHERE id = ?");
    println!(
        "# {ITERS} iters per cell, {WARMUP}-iter warm-up, {SEED_ROWS}-row seed for SELECT path"
    );
    println!();
    println!("| backend       |  ins p50 |  ins p95 |  ins p99 |  sel p50 |  sel p95 |  sel p99 |");
    println!("|---------------|---------:|---------:|---------:|---------:|---------:|---------:|");
    for r in rows {
        println!(
            "| {:<13} | {:>8.1} | {:>8.1} | {:>8.1} | {:>8.1} | {:>8.1} | {:>8.1} |",
            r.backend,
            r.insert.p50,
            r.insert.p95,
            r.insert.p99,
            r.select.p50,
            r.select.p95,
            r.select.p99,
        );
    }
    println!();
}

// ----- SPG embedded -----------------------------------------------------

fn bench_spg_embedded() -> (Stats, Stats) {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE bench_users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    // Add an index so the SELECT goes through the planner's
    // try_index_seek path — apples-to-apples with PG/MySQL/Maria's
    // PRIMARY KEY index.
    eng.execute("CREATE INDEX bench_users_id_idx ON bench_users (id)")
        .unwrap();
    for i in 1..=SEED_ROWS {
        eng.execute(&format!("INSERT INTO bench_users VALUES ({i}, 'user-{i}')"))
            .unwrap();
    }
    // Warm-up
    for i in (SEED_ROWS + 1)..(SEED_ROWS + 1 + WARMUP as i32) {
        eng.execute(&format!("INSERT INTO bench_users VALUES ({i}, 'user-{i}')"))
            .unwrap();
    }
    for _ in 0..WARMUP {
        eng.execute("SELECT id, name FROM bench_users WHERE id = 1")
            .unwrap();
    }

    let next_id = SEED_ROWS + 1 + WARMUP as i32;
    let mut insert_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = next_id + k as i32;
        let sql = format!("INSERT INTO bench_users VALUES ({id}, 'u-{id}')");
        let t0 = Instant::now();
        eng.execute(&sql).unwrap();
        insert_samples.push(t0.elapsed().as_nanos() as u64);
    }
    let mut select_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = ((k as i32) % SEED_ROWS) + 1;
        let sql = format!("SELECT id, name FROM bench_users WHERE id = {id}");
        let t0 = Instant::now();
        eng.execute(&sql).unwrap();
        select_samples.push(t0.elapsed().as_nanos() as u64);
    }
    (stats_from(insert_samples), stats_from(select_samples))
}

// ----- SPG via TCP wire -------------------------------------------------

/// Spawn `cargo run --release -p spg-server` so it binds the bench
/// port. Returns the child handle; caller kills + waits it.
fn spawn_spg_server() -> Result<Child, Box<dyn std::error::Error>> {
    // Build the binary first so we don't include compile time in the
    // server-start window. `cargo build` is idempotent if up to date.
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    // Locate the binary. CARGO_TARGET_DIR may be set by the user
    // (cargo-target-dir.md ships a wrapper that does so); fall back
    // to the per-workspace default if not.
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let mut child = Command::new(&bin)
        .arg(SPG_SERVER_ADDR)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;

    // Wait for the server to print "listening on" on stderr — only
    // then is the TCP port actually bound.
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let start = Instant::now();
    let mut line = String::new();
    while start.elapsed() < Duration::from_secs(5) {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line.contains("listening on") {
            return Ok(child);
        }
    }
    let _ = child.kill();
    Err(format!("spg-server didn't report ready in 5s; last line: {line}").into())
}

fn bench_spg_server() -> Result<(Stats, Stats), Box<dyn std::error::Error>> {
    use spg_wire::{Op, build_query, encode, parse_command_complete, parse_error_response};
    fn round_trip<W: Write, R: Read>(
        writer: &mut W,
        reader: &mut BufReader<R>,
        sql: &str,
    ) -> Result<(), String> {
        let mut out = Vec::with_capacity(64);
        encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
        writer.write_all(&out).map_err(|e| format!("write: {e}"))?;
        // Drain until CommandComplete or ErrorResponse.
        loop {
            let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
            reader
                .read_exact(&mut header)
                .map_err(|e| format!("read header: {e}"))?;
            let payload_len =
                u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
            let mut payload = vec![0u8; payload_len];
            if payload_len > 0 {
                reader
                    .read_exact(&mut payload)
                    .map_err(|e| format!("read payload: {e}"))?;
            }
            let frame = spg_wire::Frame { op, payload };
            match frame.op {
                Op::CommandComplete => {
                    let _ = parse_command_complete(&frame);
                    return Ok(());
                }
                Op::ErrorResponse | Op::Error => {
                    let msg =
                        parse_error_response(&frame).map_or("<undecodable>".into(), str::to_owned);
                    return Err(msg);
                }
                _ => {} // RowDescription / DataRow / etc — keep draining
            }
        }
    }

    let stream = TcpStream::connect(SPG_SERVER_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_nodelay(true)?; // We're measuring single round trips.
    // v3.3.1: separate write half + buffered read half so the per-
    // round-trip syscall count drops (writer = 1 write_all per req,
    // reader = 1 BufReader fill per response frame burst).
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE bench_users (id INT NOT NULL, name TEXT NOT NULL)",
    )?;
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE INDEX bench_users_id_idx ON bench_users (id)",
    )?;
    for i in 1..=SEED_ROWS {
        let sql = format!("INSERT INTO bench_users VALUES ({i}, 'user-{i}')");
        round_trip(&mut writer, &mut reader, &sql)?;
    }
    for i in (SEED_ROWS + 1)..(SEED_ROWS + 1 + WARMUP as i32) {
        let sql = format!("INSERT INTO bench_users VALUES ({i}, 'user-{i}')");
        round_trip(&mut writer, &mut reader, &sql)?;
    }
    for _ in 0..WARMUP {
        round_trip(
            &mut writer,
            &mut reader,
            "SELECT id, name FROM bench_users WHERE id = 1",
        )?;
    }

    let next_id = SEED_ROWS + 1 + WARMUP as i32;
    let mut insert_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = next_id + k as i32;
        let sql = format!("INSERT INTO bench_users VALUES ({id}, 'u-{id}')");
        let t0 = Instant::now();
        round_trip(&mut writer, &mut reader, &sql)?;
        insert_samples.push(t0.elapsed().as_nanos() as u64);
    }
    let mut select_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = ((k as i32) % SEED_ROWS) + 1;
        let sql = format!("SELECT id, name FROM bench_users WHERE id = {id}");
        let t0 = Instant::now();
        round_trip(&mut writer, &mut reader, &sql)?;
        select_samples.push(t0.elapsed().as_nanos() as u64);
    }
    Ok((stats_from(insert_samples), stats_from(select_samples)))
}

// ----- sqlx (PG / MySQL / Maria) -----------------------------------------

async fn bench_via_sqlx(pool: &AnyPool, label: &str) -> Result<(Stats, Stats), sqlx::Error> {
    // Schema per backend. PG `TEXT`, MySQL/Maria `VARCHAR(64)` are
    // semantically the closest to SPG's `TEXT NOT NULL`.
    sqlx::query("DROP TABLE IF EXISTS bench_users")
        .execute(pool)
        .await?;
    let create_sql = if label == "postgres" {
        "CREATE TABLE bench_users (id INT PRIMARY KEY, name TEXT NOT NULL)"
    } else {
        "CREATE TABLE bench_users (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL)"
    };
    sqlx::query(create_sql).execute(pool).await?;
    for i in 1..=SEED_ROWS {
        let sql = format!("INSERT INTO bench_users (id, name) VALUES ({i}, 'user-{i}')");
        sqlx::query(&sql).execute(pool).await?;
    }
    for i in (SEED_ROWS + 1)..(SEED_ROWS + 1 + WARMUP as i32) {
        let sql = format!("INSERT INTO bench_users (id, name) VALUES ({i}, 'user-{i}')");
        sqlx::query(&sql).execute(pool).await?;
    }
    for _ in 0..WARMUP {
        let _ = sqlx::query("SELECT id, name FROM bench_users WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    }

    let next_id = SEED_ROWS + 1 + WARMUP as i32;
    let mut insert_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = next_id + k as i32;
        let sql = format!("INSERT INTO bench_users (id, name) VALUES ({id}, 'u-{id}')");
        let t0 = Instant::now();
        sqlx::query(&sql).execute(pool).await?;
        insert_samples.push(t0.elapsed().as_nanos() as u64);
    }
    let mut select_samples: Vec<u64> = Vec::with_capacity(ITERS);
    for k in 0..ITERS {
        let id = ((k as i32) % SEED_ROWS) + 1;
        let sql = format!("SELECT id, name FROM bench_users WHERE id = {id}");
        let t0 = Instant::now();
        let row: Option<AnyRow> = sqlx::query(&sql).fetch_optional(pool).await?;
        let _ = row.map(|r| r.try_get::<i32, _>("id"));
        select_samples.push(t0.elapsed().as_nanos() as u64);
    }
    Ok((stats_from(insert_samples), stats_from(select_samples)))
}
