//! v4.37 scale sweep + boundary probe.
//!
//! For each backend (spg-embedded / spg-server / PG / MySQL /
//! MariaDB), incrementally grow a single table to a sequence of
//! checkpoints, then at each checkpoint measure:
//!
//!   - INSERT throughput on the segment just inserted (rows/s)
//!   - full table SCAN throughput
//!   - PK point-lookup p50 / p99 (1000 random ids)
//!   - secondary index lookup p50 / p99 (1000 random secondary
//!     keys) — reveals MySQL's double-B+tree traversal cost
//!   - RSS for the spg-server process (containers skipped — host
//!     ps doesn't see container RSS the same way and `docker
//!     stats` is async/inaccurate)
//!
//! After the main `[10K, 100K, 1M, 10M]` sweep, the boundary
//! probe continues with `[30M, 100M]`, bailing on any of:
//!
//!   - PK p99 > 100 ms  (latency cliff)
//!   - INSERT rows/s drops to < 50 % of the per-backend peak
//!   - SPG-server RSS > 4 GiB  (host RAM safety)
//!   - per-backend wall time > 15 min  (time budget)
//!
//! The bail reason is recorded in the table so the answer to
//! "where did MySQL fall over" lands directly in the output.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin sweep`

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
use std::time::{Duration, Instant};

const MAIN_CHECKPOINTS: &[usize] = &[10_000, 100_000, 1_000_000, 10_000_000];
const BOUNDARY_CHECKPOINTS: &[usize] = &[30_000_000, 100_000_000];
const BATCH: usize = 500; // rows per multi-VALUES INSERT
const LOOKUP_SAMPLES: usize = 1000;
const SPG_SERVER_ADDR: &str = "127.0.0.1:25549";

// Bail thresholds.
const PK_P99_BAIL_US: u128 = 100_000; // 100 ms
const INSERT_DROP_BAIL_RATIO: f64 = 0.5; // < 50 % of peak rows/s
const RSS_BAIL_MB: u64 = 4096; // 4 GiB
const PER_BACKEND_BUDGET_SEC: u64 = 900; // 15 min

#[derive(Debug, Clone)]
struct Sample {
    backend: String,
    n: usize,
    insert_segment_s: f64,
    insert_segment_rps: f64,
    scan_s: f64,
    scan_rps: f64,
    pk_p50_us: u128,
    pk_p99_us: u128,
    sec_p50_us: u128,
    sec_p99_us: u128,
    rss_mb: Option<u64>,
    bail: Option<String>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    println!("# Scale sweep + boundary probe");
    println!("# Checkpoints (main): {:?}", MAIN_CHECKPOINTS);
    println!("# Checkpoints (boundary): {:?}", BOUNDARY_CHECKPOINTS);
    println!(
        "# Bail: PK p99 > {} ms / INSERT < {:.0}% of peak / RSS > {} MiB / time > {} min",
        PK_P99_BAIL_US / 1000,
        INSERT_DROP_BAIL_RATIO * 100.0,
        RSS_BAIL_MB,
        PER_BACKEND_BUDGET_SEC / 60,
    );
    println!();

    let mut samples: Vec<Sample> = Vec::new();

    // --- spg-embedded ---
    eprintln!("==> spg-embedded");
    let started = Instant::now();
    run_spg_embedded(&mut samples, started)?;

    // --- spg-server ---
    eprintln!("==> spg-server");
    let mut child = spawn_spg_server()?;
    let started = Instant::now();
    if let Err(e) = run_spg_server(&mut samples, started) {
        eprintln!("  spg-server bench errored: {e}");
    }
    let _ = child.kill();
    let _ = child.wait();

    // --- competitors via sqlx ---
    for (label, url) in connection_strings() {
        eprintln!("==> {label}");
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&url)
            .await?;
        let started = Instant::now();
        if let Err(e) = run_sqlx_backend(label, &pool, &mut samples, started).await {
            eprintln!("  {label} bench errored: {e}");
        }
        pool.close().await;
    }

    println!();
    println!("## Results (per backend, per checkpoint)");
    println!();
    println!(
        "| backend       |          N | ins seg s | ins seg r/s | scan s | scan r/s | PK p50 µs | PK p99 µs | SEC p50 µs | SEC p99 µs | RSS MiB | bail |"
    );
    println!(
        "|---------------|-----------:|----------:|------------:|-------:|---------:|----------:|----------:|-----------:|-----------:|--------:|------|"
    );
    for s in &samples {
        println!(
            "| {:<13} | {:>10} | {:>9.2} | {:>11.0} | {:>6.2} | {:>8.0} | {:>9} | {:>9} | {:>10} | {:>10} | {:>7} | {} |",
            s.backend,
            fmt_n(s.n),
            s.insert_segment_s,
            s.insert_segment_rps,
            s.scan_s,
            s.scan_rps,
            s.pk_p50_us,
            s.pk_p99_us,
            s.sec_p50_us,
            s.sec_p99_us,
            s.rss_mb.map_or_else(|| "—".into(), |m| m.to_string()),
            s.bail.as_deref().unwrap_or(""),
        );
    }

    Ok(())
}

fn fmt_n(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

// ---- bail logic ----------------------------------------------------

#[derive(Default)]
struct PeakTracker {
    peak_insert_rps: f64,
}

fn bail_reason(
    sample: &Sample,
    peak: &PeakTracker,
    started: Instant,
    rss: Option<u64>,
) -> Option<String> {
    if sample.pk_p99_us > PK_P99_BAIL_US {
        return Some(format!(
            "pk p99 {} µs > {} ms cliff",
            sample.pk_p99_us,
            PK_P99_BAIL_US / 1000
        ));
    }
    // Throughput-drop bail: only meaningful after the warmup
    // checkpoints. The first two checkpoints have tiny N (10K +
    // 100K), so per-batch fsync amortization dominates → peak is
    // an artifact, not a real baseline. Compare against peak only
    // after we reach the 1M+ checkpoint.
    if sample.n >= 1_000_000
        && peak.peak_insert_rps > 0.0
        && sample.insert_segment_rps < INSERT_DROP_BAIL_RATIO * peak.peak_insert_rps
    {
        return Some(format!(
            "INSERT {:.0} r/s < {:.0}% of peak {:.0} r/s",
            sample.insert_segment_rps,
            INSERT_DROP_BAIL_RATIO * 100.0,
            peak.peak_insert_rps,
        ));
    }
    if let Some(mb) = rss
        && mb > RSS_BAIL_MB
    {
        return Some(format!("RSS {} MiB > {} MiB safety line", mb, RSS_BAIL_MB));
    }
    if started.elapsed().as_secs() > PER_BACKEND_BUDGET_SEC {
        return Some(format!(
            "per-backend budget {} min exceeded",
            PER_BACKEND_BUDGET_SEC / 60
        ));
    }
    None
}

// ---- spg-embedded --------------------------------------------------

fn run_spg_embedded(
    samples: &mut Vec<Sample>,
    started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    use spg_engine::{Engine, QueryResult};
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE sweep (id INT NOT NULL, sec INT NOT NULL, name TEXT NOT NULL)")
        .map_err(|e| e.to_string())?;
    eng.execute("CREATE INDEX sweep_id_idx ON sweep (id)")
        .map_err(|e| e.to_string())?;
    eng.execute("CREATE INDEX sweep_sec_idx ON sweep (sec)")
        .map_err(|e| e.to_string())?;
    let mut cur = 0usize;
    let mut peak = PeakTracker::default();

    for &ck in MAIN_CHECKPOINTS.iter().chain(BOUNDARY_CHECKPOINTS) {
        let seg_t = Instant::now();
        for batch_start in ((cur + 1)..=ck).step_by(BATCH) {
            let batch_end = (batch_start + BATCH).min(ck + 1);
            let mut sql = String::with_capacity(BATCH * 40);
            sql.push_str("INSERT INTO sweep (id, sec, name) VALUES ");
            for j in batch_start..batch_end {
                if j > batch_start {
                    sql.push(',');
                }
                // `sec` = id reversed via multiplication so it's a different
                // index ordering — secondary lookups can't piggyback on PK.
                let sec = ((j as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                sql.push_str(&format!("({j}, {sec}, 'u-{j}')"));
            }
            eng.execute(&sql).map_err(|e| e.to_string())?;
        }
        let seg_s = seg_t.elapsed().as_secs_f64();
        let seg_n = ck - cur;
        let seg_rps = seg_n as f64 / seg_s;
        if seg_rps > peak.peak_insert_rps {
            peak.peak_insert_rps = seg_rps;
        }
        cur = ck;

        // Scan.
        let scan_t = Instant::now();
        let scan_rows = match eng
            .execute("SELECT id FROM sweep")
            .map_err(|e| e.to_string())?
        {
            QueryResult::Rows { rows, .. } => rows.len(),
            QueryResult::CommandOk { .. } => 0,
        };
        let scan_s = scan_t.elapsed().as_secs_f64();
        let scan_rps = scan_rows as f64 / scan_s;

        // PK lookups.
        let (pk_p50, pk_p99) = {
            let mut samples_us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target = ((k as i64 * 2654435761) as usize % ck) + 1;
                let t = Instant::now();
                let _ = eng
                    .execute(&format!("SELECT name FROM sweep WHERE id = {target}"))
                    .map_err(|e| e.to_string())?;
                samples_us.push(t.elapsed().as_micros());
            }
            percentiles(&mut samples_us)
        };
        // Secondary lookups.
        let (sec_p50, sec_p99) = {
            let mut samples_us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target_id = ((k as i64 * 7853 + 13) as usize % ck) + 1;
                let target = ((target_id as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                let t = Instant::now();
                let _ = eng
                    .execute(&format!("SELECT id FROM sweep WHERE sec = {target}"))
                    .map_err(|e| e.to_string())?;
                samples_us.push(t.elapsed().as_micros());
            }
            percentiles(&mut samples_us)
        };

        let rss = process_rss_mb(std::process::id());
        let mut s = Sample {
            backend: "spg-embedded".into(),
            n: ck,
            insert_segment_s: seg_s,
            insert_segment_rps: seg_rps,
            scan_s,
            scan_rps,
            pk_p50_us: pk_p50,
            pk_p99_us: pk_p99,
            sec_p50_us: sec_p50,
            sec_p99_us: sec_p99,
            rss_mb: rss,
            bail: None,
        };
        eprintln!(
            "  [{}] ins {:.1}s ({:.0} r/s) scan {:.2}s pk_p99 {}µs sec_p99 {}µs RSS {:?}MiB",
            fmt_n(ck),
            seg_s,
            seg_rps,
            scan_s,
            pk_p99,
            sec_p99,
            rss
        );
        if let Some(b) = bail_reason(&s, &peak, started, rss) {
            s.bail = Some(b);
            samples.push(s);
            break;
        }
        samples.push(s);
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
    let tmp = std::env::temp_dir().join(format!("spg-sweep-{}", std::process::id()));
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
            // drain remaining stderr lines off-thread so the pipe
            // buffer doesn't fill mid-bench
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

fn run_spg_server(
    samples: &mut Vec<Sample>,
    started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut s = TcpStream::connect(SPG_SERVER_ADDR)?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_mins(1)))?;
    let server_pid = find_spg_server_pid();
    exec_ok(
        &mut s,
        "CREATE TABLE sweep (id INT NOT NULL, sec INT NOT NULL, name TEXT NOT NULL)",
    )?;
    exec_ok(&mut s, "CREATE INDEX sweep_id_idx ON sweep (id)")?;
    exec_ok(&mut s, "CREATE INDEX sweep_sec_idx ON sweep (sec)")?;

    let mut cur = 0usize;
    let mut peak = PeakTracker::default();
    for &ck in MAIN_CHECKPOINTS.iter().chain(BOUNDARY_CHECKPOINTS) {
        let seg_t = Instant::now();
        for batch_start in ((cur + 1)..=ck).step_by(BATCH) {
            let batch_end = (batch_start + BATCH).min(ck + 1);
            let mut sql = String::with_capacity(BATCH * 40);
            sql.push_str("INSERT INTO sweep (id, sec, name) VALUES ");
            for j in batch_start..batch_end {
                if j > batch_start {
                    sql.push(',');
                }
                let sec = ((j as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                sql.push_str(&format!("({j}, {sec}, 'u-{j}')"));
            }
            exec_ok(&mut s, &sql)?;
        }
        let seg_s = seg_t.elapsed().as_secs_f64();
        let seg_n = ck - cur;
        let seg_rps = seg_n as f64 / seg_s;
        if seg_rps > peak.peak_insert_rps {
            peak.peak_insert_rps = seg_rps;
        }
        cur = ck;

        // Scan.
        let scan_t = Instant::now();
        let scan_rows = exec_count(&mut s, "SELECT id FROM sweep")?;
        let scan_s = scan_t.elapsed().as_secs_f64();
        let scan_rps = scan_rows as f64 / scan_s;

        let (pk_p50, pk_p99) = {
            let mut samples_us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target = ((k as i64 * 2654435761) as usize % ck) + 1;
                let t = Instant::now();
                exec_count(
                    &mut s,
                    &format!("SELECT name FROM sweep WHERE id = {target}"),
                )?;
                samples_us.push(t.elapsed().as_micros());
            }
            percentiles(&mut samples_us)
        };
        let (sec_p50, sec_p99) = {
            let mut samples_us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target_id = ((k as i64 * 7853 + 13) as usize % ck) + 1;
                let target = ((target_id as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                let t = Instant::now();
                exec_count(
                    &mut s,
                    &format!("SELECT id FROM sweep WHERE sec = {target}"),
                )?;
                samples_us.push(t.elapsed().as_micros());
            }
            percentiles(&mut samples_us)
        };

        let rss = server_pid.and_then(process_rss_mb);
        let mut sample = Sample {
            backend: "spg-server".into(),
            n: ck,
            insert_segment_s: seg_s,
            insert_segment_rps: seg_rps,
            scan_s,
            scan_rps,
            pk_p50_us: pk_p50,
            pk_p99_us: pk_p99,
            sec_p50_us: sec_p50,
            sec_p99_us: sec_p99,
            rss_mb: rss,
            bail: None,
        };
        eprintln!(
            "  [{}] ins {:.1}s ({:.0} r/s) scan {:.2}s pk_p99 {}µs sec_p99 {}µs RSS {:?}MiB",
            fmt_n(ck),
            seg_s,
            seg_rps,
            scan_s,
            pk_p99,
            sec_p99,
            rss
        );
        if let Some(b) = bail_reason(&sample, &peak, started, rss) {
            sample.bail = Some(b);
            samples.push(sample);
            break;
        }
        samples.push(sample);
    }
    Ok(())
}

fn exec_ok(s: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    use spg_wire::{build_query, encode};
    let f = build_query(sql);
    let mut out = Vec::new();
    encode(&f, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    s.write_all(&out)?;
    drain_to_cc(s, sql)?;
    Ok(())
}

fn exec_count(s: &mut TcpStream, sql: &str) -> std::io::Result<usize> {
    use spg_wire::{Op, build_query, encode, parse_data_row, parse_data_row_batch};
    let f = build_query(sql);
    let mut out = Vec::new();
    encode(&f, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    s.write_all(&out)?;
    let mut total = 0usize;
    loop {
        let f = read_frame(s)?;
        match f.op {
            Op::CommandComplete => return Ok(total),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                return Err(std::io::Error::other(format!("spg-server: {sql:?}: {msg}")));
            }
            Op::DataRow => {
                let _ = parse_data_row(&f);
                total += 1;
            }
            Op::DataRowBatch => {
                if let Ok(rows) = parse_data_row_batch(&f) {
                    total += rows.len();
                }
            }
            _ => {}
        }
    }
}

fn drain_to_cc(s: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    use spg_wire::Op;
    loop {
        let f = read_frame(s)?;
        match f.op {
            Op::CommandComplete => return Ok(()),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                return Err(std::io::Error::other(format!("spg-server: {sql:?}: {msg}")));
            }
            _ => {}
        }
    }
}

fn read_frame(s: &mut TcpStream) -> std::io::Result<spg_wire::Frame> {
    use spg_wire::Op;
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header)?;
    let plen = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut payload = vec![0u8; plen];
    if plen > 0 {
        s.read_exact(&mut payload)?;
    }
    Ok(spg_wire::Frame { op, payload })
}

// ---- sqlx backends (PG / MySQL / MariaDB) -------------------------

async fn run_sqlx_backend(
    label: &'static str,
    pool: &AnyPool,
    samples: &mut Vec<Sample>,
    started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    // Schema. MySQL/MariaDB don't have UNIQUE-not-required for the
    // PK column literal, and PG / MySQL all share these forms.
    sqlx::query("DROP TABLE IF EXISTS sweep")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE sweep (id INT NOT NULL PRIMARY KEY, sec INT NOT NULL, name VARCHAR(64) NOT NULL)",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX sweep_sec_idx ON sweep (sec)")
        .execute(pool)
        .await?;

    let mut cur = 0usize;
    let mut peak = PeakTracker::default();
    for &ck in MAIN_CHECKPOINTS.iter().chain(BOUNDARY_CHECKPOINTS) {
        let seg_t = Instant::now();
        for batch_start in ((cur + 1)..=ck).step_by(BATCH) {
            let batch_end = (batch_start + BATCH).min(ck + 1);
            let mut sql = String::with_capacity(BATCH * 40);
            sql.push_str("INSERT INTO sweep (id, sec, name) VALUES ");
            for j in batch_start..batch_end {
                if j > batch_start {
                    sql.push(',');
                }
                let sec = ((j as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                sql.push_str(&format!("({j}, {sec}, 'u-{j}')"));
            }
            if let Err(e) = sqlx::query(&sql).execute(pool).await {
                eprintln!("    {label}: INSERT batch failed at id≈{batch_start}: {e}");
                let mut sample = empty_sample(label, ck);
                sample.bail = Some(format!("INSERT batch failed: {e}"));
                samples.push(sample);
                return Ok(());
            }
        }
        let seg_s = seg_t.elapsed().as_secs_f64();
        let seg_n = ck - cur;
        let seg_rps = seg_n as f64 / seg_s;
        if seg_rps > peak.peak_insert_rps {
            peak.peak_insert_rps = seg_rps;
        }
        cur = ck;

        let scan_t = Instant::now();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sweep")
            .fetch_one(pool)
            .await?;
        let scan_s = scan_t.elapsed().as_secs_f64();
        let scan_rps = row.0 as f64 / scan_s;

        let (pk_p50, pk_p99) = {
            let mut us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target = ((k as i64 * 2654435761) as usize % ck) + 1;
                let t = Instant::now();
                let _: Option<(String,)> =
                    sqlx::query_as(&format!("SELECT name FROM sweep WHERE id = {target}"))
                        .fetch_optional(pool)
                        .await
                        .ok()
                        .flatten();
                us.push(t.elapsed().as_micros());
            }
            percentiles(&mut us)
        };
        let (sec_p50, sec_p99) = {
            let mut us: Vec<u128> = Vec::with_capacity(LOOKUP_SAMPLES);
            for k in 0..LOOKUP_SAMPLES {
                let target_id = ((k as i64 * 7853 + 13) as usize % ck) + 1;
                let target = ((target_id as u64).wrapping_mul(2654435761) % 1_000_000_000) as i32;
                let t = Instant::now();
                let _: Option<(i64,)> =
                    sqlx::query_as(&format!("SELECT id FROM sweep WHERE sec = {target}"))
                        .fetch_optional(pool)
                        .await
                        .ok()
                        .flatten();
                us.push(t.elapsed().as_micros());
            }
            percentiles(&mut us)
        };

        let mut sample = Sample {
            backend: label.into(),
            n: ck,
            insert_segment_s: seg_s,
            insert_segment_rps: seg_rps,
            scan_s,
            scan_rps,
            pk_p50_us: pk_p50,
            pk_p99_us: pk_p99,
            sec_p50_us: sec_p50,
            sec_p99_us: sec_p99,
            rss_mb: None,
            bail: None,
        };
        eprintln!(
            "  [{}] ins {:.1}s ({:.0} r/s) scan {:.2}s pk_p99 {}µs sec_p99 {}µs",
            fmt_n(ck),
            seg_s,
            seg_rps,
            scan_s,
            pk_p99,
            sec_p99
        );
        if let Some(b) = bail_reason(&sample, &peak, started, None) {
            sample.bail = Some(b);
            samples.push(sample);
            break;
        }
        samples.push(sample);
    }
    Ok(())
}

fn empty_sample(label: &str, n: usize) -> Sample {
    Sample {
        backend: label.into(),
        n,
        insert_segment_s: 0.0,
        insert_segment_rps: 0.0,
        scan_s: 0.0,
        scan_rps: 0.0,
        pk_p50_us: 0,
        pk_p99_us: 0,
        sec_p50_us: 0,
        sec_p99_us: 0,
        rss_mb: None,
        bail: None,
    }
}

// ---- helpers ------------------------------------------------------

fn percentiles(us: &mut [u128]) -> (u128, u128) {
    us.sort_unstable();
    let n = us.len();
    let p50 = us[((n as f64) * 0.50) as usize];
    let p99 = us[(((n as f64) * 0.99) as usize).min(n - 1)];
    (p50, p99)
}

fn process_rss_mb(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib / 1024)
}

fn find_spg_server_pid() -> Option<u32> {
    // best-effort: pgrep -f the bound addr; if pgrep isn't there
    // (rare) we just skip RSS for the run.
    let out = Command::new("pgrep")
        .args(["-f", &format!("spg-server {SPG_SERVER_ADDR}")])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
}
