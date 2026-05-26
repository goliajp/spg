//! Throughput bench — bulk INSERT (10000 rows in 100-row VALUES
//! batches) + full-table SELECT scan. All five backends, same shape.
//!
//! Run:  cargo run --release -p spg-bench-competitor --bin throughput

// Bench-code allow-list. See latency.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::match_wildcard_for_single_variants,
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

const TOTAL_ROWS: usize = 10_000;
const BATCH_SIZE: usize = 100; // rows per multi-VALUES INSERT statement
const SPG_SERVER_ADDR: &str = "127.0.0.1:25545";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let mut rows: Vec<RowRes> = Vec::new();

    // SPG embedded
    let r = bench_spg_embedded();
    rows.push(RowRes {
        backend: "spg-embedded".into(),
        ..r
    });

    // SPG server (TCP wire)
    {
        let mut child = spawn_spg_server()?;
        let r = bench_spg_server()?;
        let _ = child.kill();
        let _ = child.wait();
        rows.push(RowRes {
            backend: "spg-server".into(),
            ..r
        });
    }

    // PG / MySQL / Maria
    for (label, url) in connection_strings() {
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await?;
        let r = bench_via_sqlx(&pool, label).await?;
        pool.close().await;
        rows.push(RowRes {
            backend: label.into(),
            ..r
        });
    }

    print_table(&rows);
    Ok(())
}

#[derive(Default, Clone)]
struct RowRes {
    backend: String,
    insert_total_ms: f64,
    insert_rows_per_sec: f64,
    scan_total_ms: f64,
    scan_rows_per_sec: f64,
}

fn print_table(rs: &[RowRes]) {
    println!();
    println!(
        "# throughput — {} rows, {} per multi-VALUES INSERT, then full SELECT scan",
        TOTAL_ROWS, BATCH_SIZE
    );
    println!();
    println!("| backend       |  INSERT ms |     INS rows/s |   SCAN ms |    SCAN rows/s |");
    println!("|---------------|-----------:|---------------:|----------:|---------------:|");
    for r in rs {
        println!(
            "| {:<13} | {:>10.2} | {:>14.0} | {:>9.2} | {:>14.0} |",
            r.backend,
            r.insert_total_ms,
            r.insert_rows_per_sec,
            r.scan_total_ms,
            r.scan_rows_per_sec,
        );
    }
    println!();
}

fn make_batch_sql(start_id: usize, count: usize) -> String {
    let mut s = String::from("INSERT INTO bench_users (id, name) VALUES ");
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        let id = start_id + i;
        s.push_str(&format!("({id}, 'u-{id}')"));
    }
    s
}

// ----- SPG embedded -----------------------------------------------------

fn bench_spg_embedded() -> RowRes {
    use spg_engine::{Engine, QueryResult};
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE bench_users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();

    let t0 = Instant::now();
    let mut id = 1usize;
    while id <= TOTAL_ROWS {
        let count = BATCH_SIZE.min(TOTAL_ROWS - id + 1);
        let sql = make_batch_sql(id, count);
        eng.execute(&sql).unwrap();
        id += count;
    }
    let insert_total = t0.elapsed();

    let t1 = Instant::now();
    let result = eng.execute("SELECT id, name FROM bench_users").unwrap();
    let scan_total = t1.elapsed();
    let scan_row_count = match result {
        QueryResult::Rows { ref rows, .. } => rows.len(),
        _ => 0,
    };
    assert_eq!(scan_row_count, TOTAL_ROWS, "scan returned wrong row count");

    RowRes {
        backend: String::new(),
        insert_total_ms: insert_total.as_secs_f64() * 1000.0,
        insert_rows_per_sec: TOTAL_ROWS as f64 / insert_total.as_secs_f64(),
        scan_total_ms: scan_total.as_secs_f64() * 1000.0,
        scan_rows_per_sec: TOTAL_ROWS as f64 / scan_total.as_secs_f64(),
    }
}

// ----- SPG server (TCP) -------------------------------------------------

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

fn bench_spg_server() -> Result<RowRes, Box<dyn std::error::Error>> {
    use spg_wire::{Op, build_query, encode, parse_command_complete, parse_error_response};
    fn round_trip(stream: &mut TcpStream, sql: &str) -> Result<usize, String> {
        let mut out = Vec::with_capacity(sql.len() + 16);
        encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
        stream.write_all(&out).map_err(|e| format!("write: {e}"))?;
        let mut row_count = 0usize;
        loop {
            let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
            stream
                .read_exact(&mut header)
                .map_err(|e| format!("read header: {e}"))?;
            let payload_len =
                u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
            let mut payload = vec![0u8; payload_len];
            if payload_len > 0 {
                stream
                    .read_exact(&mut payload)
                    .map_err(|e| format!("read payload: {e}"))?;
            }
            let frame = spg_wire::Frame { op, payload };
            match frame.op {
                Op::DataRow => row_count += 1,
                Op::DataRowBatch if frame.payload.len() >= 2 => {
                    // v3.3.0 batched rows — peek the leading u16
                    // row_count without fully decoding the payload.
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

    let mut stream = TcpStream::connect(SPG_SERVER_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_mins(1)))?;
    stream.set_nodelay(true)?;
    round_trip(
        &mut stream,
        "CREATE TABLE bench_users (id INT NOT NULL, name TEXT NOT NULL)",
    )
    .map_err(|e| format!("create: {e}"))?;

    let t0 = Instant::now();
    let mut id = 1usize;
    while id <= TOTAL_ROWS {
        let count = BATCH_SIZE.min(TOTAL_ROWS - id + 1);
        let sql = make_batch_sql(id, count);
        round_trip(&mut stream, &sql).map_err(|e| format!("insert: {e}"))?;
        id += count;
    }
    let insert_total = t0.elapsed();

    let t1 = Instant::now();
    let scan_row_count = round_trip(&mut stream, "SELECT id, name FROM bench_users")
        .map_err(|e| format!("scan: {e}"))?;
    let scan_total = t1.elapsed();
    assert_eq!(scan_row_count, TOTAL_ROWS, "scan returned wrong row count");

    Ok(RowRes {
        backend: String::new(),
        insert_total_ms: insert_total.as_secs_f64() * 1000.0,
        insert_rows_per_sec: TOTAL_ROWS as f64 / insert_total.as_secs_f64(),
        scan_total_ms: scan_total.as_secs_f64() * 1000.0,
        scan_rows_per_sec: TOTAL_ROWS as f64 / scan_total.as_secs_f64(),
    })
}

// ----- sqlx (PG / MySQL / Maria) ----------------------------------------

async fn bench_via_sqlx(pool: &AnyPool, label: &str) -> Result<RowRes, sqlx::Error> {
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
    while id <= TOTAL_ROWS {
        let count = BATCH_SIZE.min(TOTAL_ROWS - id + 1);
        let sql = make_batch_sql(id, count);
        sqlx::query(&sql).execute(pool).await?;
        id += count;
    }
    let insert_total = t0.elapsed();

    let t1 = Instant::now();
    let recs = sqlx::query("SELECT id, name FROM bench_users")
        .fetch_all(pool)
        .await?;
    let scan_total = t1.elapsed();
    assert_eq!(recs.len(), TOTAL_ROWS, "scan returned wrong row count");

    Ok(RowRes {
        backend: String::new(),
        insert_total_ms: insert_total.as_secs_f64() * 1000.0,
        insert_rows_per_sec: TOTAL_ROWS as f64 / insert_total.as_secs_f64(),
        scan_total_ms: scan_total.as_secs_f64() * 1000.0,
        scan_rows_per_sec: TOTAL_ROWS as f64 / scan_total.as_secs_f64(),
    })
}
