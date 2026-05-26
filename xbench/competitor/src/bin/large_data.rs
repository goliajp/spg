//! Large-data report.
//!
//! * SPG embedded + server: 1M-row INSERT + full scan + indexed
//!   lookup + 100K dim-128 vector HNSW build + query.
//! * PG / MySQL / MariaDB: same workload at 1/10 scale (100K rows,
//!   10K vectors) so the run fits in a few minutes. Ratios are
//!   apples-to-apples within a row.
//!
//! Reports RSS for the SPG server after each phase.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin large_data`

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::match_wildcard_for_single_variants,
    clippy::needless_range_loop,
    clippy::similar_names,
    clippy::suspicious_map,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion
)]

use spg_bench_competitor::connection_strings;
use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Row};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SPG_ROWS: usize = 1_000_000;
const SPG_VECTORS: usize = 100_000;
const COMP_ROWS: usize = 100_000;
const COMP_VECTORS: usize = 10_000;
const DIM: usize = 128;
const BATCH: usize = 500; // multi-VALUES INSERT batch size
const QUERIES: usize = 200;
const SPG_SERVER_ADDR: &str = "127.0.0.1:25548";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    println!("# Large-data report");
    println!(
        "# (SPG scale: {} rows / {} dim-{} vectors; competitor scale: {} rows / {} vectors)",
        SPG_ROWS, SPG_VECTORS, DIM, COMP_ROWS, COMP_VECTORS
    );
    println!();

    // -- SPG embedded --
    let r = bench_spg_embedded();
    println!(
        "## spg-embedded ({} rows / {} dim-{} vectors)",
        SPG_ROWS, SPG_VECTORS, DIM
    );
    print_report(&r);
    println!();

    // -- SPG server --
    {
        let mut child = spawn_spg_server()?;
        let r = bench_spg_server()?;
        let _ = child.kill();
        let _ = child.wait();
        println!(
            "## spg-server ({} rows / {} dim-{} vectors, RSS sampled)",
            SPG_ROWS, SPG_VECTORS, DIM
        );
        print_report(&r);
        println!();
    }

    // -- Competitors at 10× smaller scale --
    println!(
        "## PG / MySQL / MariaDB ({} rows; vectors only for pgvector at {})",
        COMP_ROWS, COMP_VECTORS
    );
    println!();
    println!(
        "| backend           |  ins ms |   ins r/s |  scan ms |  scan r/s |  pk-lookup µs |  hnsw build s |  hnsw q p50 µs |"
    );
    println!(
        "|-------------------|--------:|----------:|---------:|----------:|--------------:|--------------:|---------------:|"
    );
    for (label, url) in connection_strings() {
        let r = bench_via_sqlx(
            &AnyPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(10))
                .connect(&url)
                .await?,
            label,
            COMP_ROWS,
        )
        .await?;
        let (hnsw_build_s, hnsw_q50_us) = if label == "postgres" {
            let pool = AnyPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(10))
                .connect(&url)
                .await?;
            let r = bench_pgvector(&pool, COMP_VECTORS).await?;
            pool.close().await;
            (r.0, r.1)
        } else {
            (0.0, 0.0)
        };
        println!(
            "| {:<17} | {:>7.1} | {:>9.0} | {:>8.1} | {:>9.0} | {:>13.1} | {:>13.2} | {:>14.1} |",
            label,
            r.insert_ms,
            r.insert_rps,
            r.scan_ms,
            r.scan_rps,
            r.pk_lookup_us,
            hnsw_build_s,
            hnsw_q50_us,
        );
    }
    println!();

    Ok(())
}

#[derive(Default, Clone, Debug)]
struct Report {
    insert_ms: f64,
    insert_rps: f64,
    scan_ms: f64,
    scan_rps: f64,
    pk_lookup_us: f64,
    hnsw_build_s: f64,
    hnsw_q50_us: f64,
    rss_after_rows_kib: i64,
    rss_after_hnsw_kib: i64,
}

fn print_report(r: &Report) {
    println!("| op                          |       value |");
    println!("|-----------------------------|------------:|");
    println!("| INSERT rows total           | {:>8.1} ms |", r.insert_ms);
    println!(
        "| INSERT throughput           | {:>8.0} r/s |",
        r.insert_rps
    );
    println!("| SCAN full table             | {:>8.1} ms |", r.scan_ms);
    println!("| SCAN throughput             | {:>8.0} r/s |", r.scan_rps);
    println!(
        "| WHERE id = X p50            | {:>8.1} µs |",
        r.pk_lookup_us
    );
    println!(
        "| HNSW build (100K dim-128)   | {:>8.2} s  |",
        r.hnsw_build_s
    );
    println!(
        "| HNSW kNN q p50              | {:>8.1} µs |",
        r.hnsw_q50_us
    );
    if r.rss_after_rows_kib > 0 {
        println!(
            "| RSS after row load          | {:>8.1} MiB |",
            r.rss_after_rows_kib as f64 / 1024.0
        );
    }
    if r.rss_after_hnsw_kib > 0 {
        println!(
            "| RSS after HNSW              | {:>8.1} MiB |",
            r.rss_after_hnsw_kib as f64 / 1024.0
        );
    }
}

// ----- SPG embedded -----------------------------------------------------

fn bench_spg_embedded() -> Report {
    use spg_engine::{Engine, QueryResult};
    let mut r = Report::default();
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX users_id_idx ON users (id)")
        .unwrap();

    // INSERT 1M rows in BATCH-row VALUES batches.
    let t0 = Instant::now();
    let mut id = 1usize;
    while id <= SPG_ROWS {
        let mut sql = String::with_capacity(BATCH * 32);
        sql.push_str("INSERT INTO users (id, name) VALUES ");
        let end = (id + BATCH).min(SPG_ROWS + 1);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        eng.execute(&sql).unwrap();
        id = end;
    }
    r.insert_ms = t0.elapsed().as_secs_f64() * 1000.0;
    r.insert_rps = SPG_ROWS as f64 / t0.elapsed().as_secs_f64();

    // SCAN full table.
    let t1 = Instant::now();
    let res = eng.execute("SELECT id, name FROM users").unwrap();
    r.scan_ms = t1.elapsed().as_secs_f64() * 1000.0;
    r.scan_rps = SPG_ROWS as f64 / t1.elapsed().as_secs_f64();
    if let QueryResult::Rows { rows, .. } = res {
        assert_eq!(rows.len(), SPG_ROWS);
    }

    // PK lookup p50 over QUERIES samples.
    let mut lookup_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let target = ((k as i32) * 37 % SPG_ROWS as i32) + 1;
        let sql = format!("SELECT id, name FROM users WHERE id = {target}");
        let t = Instant::now();
        eng.execute(&sql).unwrap();
        lookup_samples.push(t.elapsed().as_nanos() as u64);
    }
    lookup_samples.sort_unstable();
    r.pk_lookup_us = lookup_samples[lookup_samples.len() / 2] as f64 / 1000.0;

    // HNSW: 100K dim-128 vectors + index + queries.
    eng.execute("CREATE TABLE vecs (id INT NOT NULL, v VECTOR(128) NOT NULL)")
        .unwrap();
    let t2 = Instant::now();
    for i in 0..SPG_VECTORS {
        let mut sql = format!("INSERT INTO vecs VALUES ({i}, [");
        for d in 0..DIM {
            if d > 0 {
                sql.push(',');
            }
            let f = ((i * 31 + d * 7) % 997) as f32 * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("])");
        eng.execute(&sql).unwrap();
    }
    eng.execute("CREATE INDEX vecs_idx ON vecs USING hnsw (v)")
        .unwrap();
    r.hnsw_build_s = t2.elapsed().as_secs_f64();

    let mut q_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let mut sql = String::from("SELECT id FROM vecs ORDER BY v <-> [");
        for d in 0..DIM {
            if d > 0 {
                sql.push(',');
            }
            let f = ((k * 13 + d * 5) % 997) as f32 * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("] LIMIT 10");
        let t = Instant::now();
        eng.execute(&sql).unwrap();
        q_samples.push(t.elapsed().as_nanos() as u64);
    }
    q_samples.sort_unstable();
    r.hnsw_q50_us = q_samples[q_samples.len() / 2] as f64 / 1000.0;
    r
}

// ----- SPG server -------------------------------------------------------

fn bench_spg_server() -> Result<Report, Box<dyn std::error::Error>> {
    let mut r = Report::default();
    let pid = SERVER_PID.load(std::sync::atomic::Ordering::Relaxed);

    let stream = TcpStream::connect(SPG_SERVER_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_mins(10)))?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);

    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)",
    )?;
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE INDEX users_id_idx ON users (id)",
    )?;

    let t0 = Instant::now();
    let mut id = 1usize;
    while id <= SPG_ROWS {
        let mut sql = String::with_capacity(BATCH * 32);
        sql.push_str("INSERT INTO users (id, name) VALUES ");
        let end = (id + BATCH).min(SPG_ROWS + 1);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        round_trip(&mut writer, &mut reader, &sql)?;
        id = end;
    }
    r.insert_ms = t0.elapsed().as_secs_f64() * 1000.0;
    r.insert_rps = SPG_ROWS as f64 / t0.elapsed().as_secs_f64();
    r.rss_after_rows_kib = rss_kib(pid);

    let t1 = Instant::now();
    let scan_rows = round_trip(&mut writer, &mut reader, "SELECT id, name FROM users")?;
    r.scan_ms = t1.elapsed().as_secs_f64() * 1000.0;
    r.scan_rps = SPG_ROWS as f64 / t1.elapsed().as_secs_f64();
    assert_eq!(scan_rows, SPG_ROWS);

    let mut lookup_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let target = ((k as i32) * 37 % SPG_ROWS as i32) + 1;
        let sql = format!("SELECT id, name FROM users WHERE id = {target}");
        let t = Instant::now();
        round_trip(&mut writer, &mut reader, &sql)?;
        lookup_samples.push(t.elapsed().as_nanos() as u64);
    }
    lookup_samples.sort_unstable();
    r.pk_lookup_us = lookup_samples[lookup_samples.len() / 2] as f64 / 1000.0;

    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE vecs (id INT NOT NULL, v VECTOR(128) NOT NULL)",
    )?;
    let t2 = Instant::now();
    for i in 0..SPG_VECTORS {
        let mut sql = format!("INSERT INTO vecs VALUES ({i}, [");
        for d in 0..DIM {
            if d > 0 {
                sql.push(',');
            }
            let f = ((i * 31 + d * 7) % 997) as f32 * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("])");
        round_trip(&mut writer, &mut reader, &sql)?;
    }
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE INDEX vecs_idx ON vecs USING hnsw (v)",
    )?;
    r.hnsw_build_s = t2.elapsed().as_secs_f64();
    r.rss_after_hnsw_kib = rss_kib(pid);

    let mut q_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let mut sql = String::from("SELECT id FROM vecs ORDER BY v <-> [");
        for d in 0..DIM {
            if d > 0 {
                sql.push(',');
            }
            let f = ((k * 13 + d * 5) % 997) as f32 * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("] LIMIT 10");
        let t = Instant::now();
        round_trip(&mut writer, &mut reader, &sql)?;
        q_samples.push(t.elapsed().as_nanos() as u64);
    }
    q_samples.sort_unstable();
    r.hnsw_q50_us = q_samples[q_samples.len() / 2] as f64 / 1000.0;
    Ok(r)
}

static SERVER_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn spawn_spg_server() -> Result<Child, Box<dyn std::error::Error>> {
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let mut child = Command::new(&bin)
        .arg(SPG_SERVER_ADDR)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    SERVER_PID.store(child.id(), std::sync::atomic::Ordering::Relaxed);
    let stderr = child.stderr.take().expect("piped");
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
    Err("spg-server didn't report ready in 5s".into())
}

fn round_trip<W: Write, R: Read>(
    writer: &mut W,
    reader: &mut BufReader<R>,
    sql: &str,
) -> Result<usize, String> {
    use spg_wire::{Op, build_query, encode, parse_command_complete, parse_error_response};
    let mut out = Vec::with_capacity(sql.len() + 16);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    writer.write_all(&out).map_err(|e| format!("write: {e}"))?;
    let mut row_count = 0usize;
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        reader
            .read_exact(&mut header)
            .map_err(|e| format!("read header: {e}"))?;
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader
                .read_exact(&mut payload)
                .map_err(|e| format!("read payload: {e}"))?;
        }
        let frame = spg_wire::Frame { op, payload };
        match frame.op {
            Op::DataRow => row_count += 1,
            Op::DataRowBatch if frame.payload.len() >= 2 => {
                row_count += u16::from_le_bytes([frame.payload[0], frame.payload[1]]) as usize;
            }
            Op::CommandComplete => {
                let _ = parse_command_complete(&frame);
                return Ok(row_count);
            }
            Op::ErrorResponse | Op::Error => {
                let msg =
                    parse_error_response(&frame).map_or("<undecodable>".into(), str::to_owned);
                return Err(msg);
            }
            _ => {}
        }
    }
}

fn rss_kib(pid: u32) -> i64 {
    if pid == 0 {
        return 0;
    }
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<i64>()
            .unwrap_or(0),
        _ => 0,
    }
}

// ----- sqlx competitors (10× smaller scale) -----------------------------

struct CompReport {
    insert_ms: f64,
    insert_rps: f64,
    scan_ms: f64,
    scan_rps: f64,
    pk_lookup_us: f64,
}

async fn bench_via_sqlx(
    pool: &AnyPool,
    label: &str,
    rows: usize,
) -> Result<CompReport, sqlx::Error> {
    sqlx::query("DROP TABLE IF EXISTS bench_users")
        .execute(pool)
        .await?;
    let create_sql = if label == "postgres" {
        "CREATE TABLE bench_users (id INT PRIMARY KEY, name TEXT NOT NULL)"
    } else {
        "CREATE TABLE bench_users (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL)"
    };
    sqlx::query(create_sql).execute(pool).await?;

    let t0 = Instant::now();
    let mut id = 1usize;
    while id <= rows {
        let mut sql = String::with_capacity(BATCH * 32);
        sql.push_str("INSERT INTO bench_users (id, name) VALUES ");
        let end = (id + BATCH).min(rows + 1);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        sqlx::query(&sql).execute(pool).await?;
        id = end;
    }
    let insert_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let insert_rps = rows as f64 / t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let recs = sqlx::query("SELECT id, name FROM bench_users")
        .fetch_all(pool)
        .await?;
    let scan_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let scan_rps = rows as f64 / t1.elapsed().as_secs_f64();
    assert_eq!(recs.len(), rows);

    let mut lookup_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let target = ((k as i32) * 37 % rows as i32) + 1;
        let sql = format!("SELECT id, name FROM bench_users WHERE id = {target}");
        let t = Instant::now();
        let _ = sqlx::query(&sql).fetch_optional(pool).await?;
        lookup_samples.push(t.elapsed().as_nanos() as u64);
    }
    lookup_samples.sort_unstable();
    let pk_lookup_us = lookup_samples[lookup_samples.len() / 2] as f64 / 1000.0;

    Ok(CompReport {
        insert_ms,
        insert_rps,
        scan_ms,
        scan_rps,
        pk_lookup_us,
    })
}

async fn bench_pgvector(
    pool: &AnyPool,
    n: usize,
) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(pool)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS vecs")
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "CREATE TABLE vecs (id INT PRIMARY KEY, v vector({}))",
        DIM
    ))
    .execute(pool)
    .await?;

    let t0 = Instant::now();
    let mut i = 0;
    while i < n {
        let mut sql = String::with_capacity(BATCH * 32);
        sql.push_str("INSERT INTO vecs (id, v) VALUES ");
        let end = (i + 100).min(n);
        for j in i..end {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!("({}, '[", j));
            for d in 0..DIM {
                if d > 0 {
                    sql.push(',');
                }
                let f = ((j * 31 + d * 7) % 997) as f32 * 0.001;
                sql.push_str(&format!("{:.4}", f));
            }
            sql.push_str("]')");
        }
        sqlx::query(&sql).execute(pool).await?;
        i = end;
    }
    sqlx::query("CREATE INDEX vecs_idx ON vecs USING hnsw (v vector_l2_ops)")
        .execute(pool)
        .await?;
    let build_s = t0.elapsed().as_secs_f64();

    let mut q_samples: Vec<u64> = Vec::with_capacity(QUERIES);
    for k in 0..QUERIES {
        let mut sql = String::from("SELECT id FROM vecs ORDER BY v <-> '[");
        for d in 0..DIM {
            if d > 0 {
                sql.push(',');
            }
            let f = ((k * 13 + d * 5) % 997) as f32 * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("]' LIMIT 10");
        let t = Instant::now();
        let rows = sqlx::query(&sql).fetch_all(pool).await?;
        let _ = rows.iter().map(|r| r.try_get::<i32, _>("id")).count();
        q_samples.push(t.elapsed().as_nanos() as u64);
    }
    q_samples.sort_unstable();
    Ok((build_s, q_samples[q_samples.len() / 2] as f64 / 1000.0))
}
