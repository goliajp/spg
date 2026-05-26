//! Vector kNN bench — HNSW search top-10 over 10K dim-128 vectors.
//! SPG embedded + SPG server + Postgres + pgvector. MySQL and MariaDB
//! have no native vector index, so they're skipped (a brute-force
//! ORDER BY would test a different thing entirely).
//!
//! Run:  cargo run --release -p spg-bench-competitor --bin vector_knn

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
    clippy::suspicious_map,
    clippy::needless_range_loop,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion
)]

use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Row};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DIM: usize = 128;
const N_VECTORS: usize = 10_000;
const K: usize = 10;
const WARMUP_QUERIES: usize = 50;
const MEASURE_QUERIES: usize = 500;
const SPG_SERVER_ADDR: &str = "127.0.0.1:25546";
const PG_URL: &str = "postgres://bench:bench@127.0.0.1:25432/bench";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let vectors = gen_vectors(N_VECTORS, DIM, 0xCAFE_BABE);
    let queries = gen_vectors(WARMUP_QUERIES + MEASURE_QUERIES, DIM, 0xDEAD_BEEF);

    let mut rows: Vec<KnnRes> = Vec::new();

    // SPG embedded — direct Engine; no wire, no fsync.
    let r = bench_spg_embedded(&vectors, &queries);
    rows.push(KnnRes {
        backend: "spg-embedded".into(),
        ..r
    });

    // SPG server (TCP wire).
    {
        let mut child = spawn_spg_server()?;
        let r = bench_spg_server(&vectors, &queries)?;
        let _ = child.kill();
        let _ = child.wait();
        rows.push(KnnRes {
            backend: "spg-server".into(),
            ..r
        });
    }

    // Postgres + pgvector (already in the docker image).
    {
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect(PG_URL)
            .await?;
        let r = bench_pgvector(&pool, &vectors, &queries).await?;
        pool.close().await;
        rows.push(KnnRes {
            backend: "postgres+pgvector".into(),
            ..r
        });
    }

    print_table(&rows);
    Ok(())
}

#[derive(Default, Clone)]
struct KnnRes {
    backend: String,
    build_total_s: f64,
    query_p50_us: f64,
    query_p95_us: f64,
    query_p99_us: f64,
}

fn print_table(rs: &[KnnRes]) {
    println!();
    println!(
        "# vector kNN — top-{} over {} dim-{} vectors (HNSW where available)",
        K, N_VECTORS, DIM
    );
    println!(
        "# bulk-build time + per-query latency from {} measured queries",
        MEASURE_QUERIES
    );
    println!();
    println!("| backend             |  build s |  q p50 µs |  q p95 µs |  q p99 µs |");
    println!("|---------------------|---------:|----------:|----------:|----------:|");
    for r in rs {
        println!(
            "| {:<19} | {:>8.2} | {:>9.1} | {:>9.1} | {:>9.1} |",
            r.backend, r.build_total_s, r.query_p50_us, r.query_p95_us, r.query_p99_us,
        );
    }
    println!();
}

fn pct(samples: &mut [u64], p: f64) -> f64 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * p / 100.0).clamp(0.0, samples.len() as f64 - 1.0);
    samples[idx as usize] as f64 / 1000.0 // µs
}

/// Deterministic LCG-based pseudo-random f32 vectors in [-1, 1]. Same
/// seed → same vectors, so SPG and pgvector see identical data.
fn gen_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let bits = ((state >> 32) as u32) & 0x00FF_FFFF;
            // Map to [-1, 1].
            let f = (bits as f32) / (0x0080_0000 as f32) - 1.0;
            v.push(f);
        }
        out.push(v);
    }
    out
}

fn vec_to_spg_literal(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        // SPG accepts both integer and decimal forms; force a dot so
        // it's parsed as Float lane.
        s.push_str(&format!("{:.6}", x));
    }
    s.push(']');
    s
}

fn vec_to_pgvector_literal(v: &[f32]) -> String {
    // pgvector text format is `'[1,2,3]'::vector`. We build the
    // `'[1,2,3]'` string (sqlx will pass it as a text bind that the
    // server casts to vector via the column type).
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{:.6}", x));
    }
    s.push(']');
    s
}

// ----- SPG embedded -----------------------------------------------------

fn bench_spg_embedded(vectors: &[Vec<f32>], queries: &[Vec<f32>]) -> KnnRes {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute(&format!(
        "CREATE TABLE vecs (id INT NOT NULL, v VECTOR({}) NOT NULL)",
        DIM
    ))
    .unwrap();
    let t0 = Instant::now();
    for (i, v) in vectors.iter().enumerate() {
        let sql = format!("INSERT INTO vecs VALUES ({}, {})", i, vec_to_spg_literal(v));
        eng.execute(&sql).unwrap();
    }
    eng.execute("CREATE INDEX vecs_idx ON vecs USING hnsw (v)")
        .unwrap();
    let build_total = t0.elapsed();

    // Warm-up
    for q in &queries[..WARMUP_QUERIES] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> {} LIMIT {}",
            vec_to_spg_literal(q),
            K
        );
        eng.execute(&sql).unwrap();
    }
    let mut samples: Vec<u64> = Vec::with_capacity(MEASURE_QUERIES);
    for q in &queries[WARMUP_QUERIES..] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> {} LIMIT {}",
            vec_to_spg_literal(q),
            K
        );
        let t0 = Instant::now();
        eng.execute(&sql).unwrap();
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    KnnRes {
        backend: String::new(),
        build_total_s: build_total.as_secs_f64(),
        query_p50_us: pct(&mut samples.clone(), 50.0),
        query_p95_us: pct(&mut samples.clone(), 95.0),
        query_p99_us: pct(&mut samples, 99.0),
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

fn bench_spg_server(
    vectors: &[Vec<f32>],
    queries: &[Vec<f32>],
) -> Result<KnnRes, Box<dyn std::error::Error>> {
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
    stream.set_read_timeout(Some(Duration::from_mins(2)))?;
    stream.set_nodelay(true)?;

    round_trip(
        &mut stream,
        &format!(
            "CREATE TABLE vecs (id INT NOT NULL, v VECTOR({}) NOT NULL)",
            DIM
        ),
    )
    .map_err(|e| format!("create: {e}"))?;

    let t0 = Instant::now();
    for (i, v) in vectors.iter().enumerate() {
        let sql = format!("INSERT INTO vecs VALUES ({}, {})", i, vec_to_spg_literal(v));
        round_trip(&mut stream, &sql).map_err(|e| format!("insert: {e}"))?;
    }
    round_trip(&mut stream, "CREATE INDEX vecs_idx ON vecs USING hnsw (v)")
        .map_err(|e| format!("create index: {e}"))?;
    let build_total = t0.elapsed();

    for q in &queries[..WARMUP_QUERIES] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> {} LIMIT {}",
            vec_to_spg_literal(q),
            K
        );
        round_trip(&mut stream, &sql).map_err(|e| format!("warm query: {e}"))?;
    }
    let mut samples: Vec<u64> = Vec::with_capacity(MEASURE_QUERIES);
    for q in &queries[WARMUP_QUERIES..] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> {} LIMIT {}",
            vec_to_spg_literal(q),
            K
        );
        let t0 = Instant::now();
        round_trip(&mut stream, &sql).map_err(|e| format!("measure: {e}"))?;
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    Ok(KnnRes {
        backend: String::new(),
        build_total_s: build_total.as_secs_f64(),
        query_p50_us: pct(&mut samples.clone(), 50.0),
        query_p95_us: pct(&mut samples.clone(), 95.0),
        query_p99_us: pct(&mut samples, 99.0),
    })
}

// ----- pgvector ---------------------------------------------------------

async fn bench_pgvector(
    pool: &AnyPool,
    vectors: &[Vec<f32>],
    queries: &[Vec<f32>],
) -> Result<KnnRes, Box<dyn std::error::Error>> {
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
    // Bulk insert in 100-row batches for speed.
    let mut i = 0;
    while i < vectors.len() {
        let mut sql = String::from("INSERT INTO vecs (id, v) VALUES ");
        let end = (i + 100).min(vectors.len());
        for j in i..end {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, '{}')",
                j,
                vec_to_pgvector_literal(&vectors[j])
            ));
        }
        sqlx::query(&sql).execute(pool).await?;
        i = end;
    }
    // Build the HNSW index after load (idiomatic pgvector — building
    // after load is much faster than maintaining on-INSERT).
    sqlx::query("CREATE INDEX vecs_idx ON vecs USING hnsw (v vector_l2_ops)")
        .execute(pool)
        .await?;
    let build_total = t0.elapsed();

    for q in &queries[..WARMUP_QUERIES] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> '{}' LIMIT {}",
            vec_to_pgvector_literal(q),
            K
        );
        let _ = sqlx::query(&sql).fetch_all(pool).await?;
    }
    let mut samples: Vec<u64> = Vec::with_capacity(MEASURE_QUERIES);
    for q in &queries[WARMUP_QUERIES..] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY v <-> '{}' LIMIT {}",
            vec_to_pgvector_literal(q),
            K
        );
        let t0 = Instant::now();
        let rows = sqlx::query(&sql).fetch_all(pool).await?;
        let _ = rows.iter().map(|r| r.try_get::<i32, _>("id")).count();
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    Ok(KnnRes {
        backend: String::new(),
        build_total_s: build_total.as_secs_f64(),
        query_p50_us: pct(&mut samples.clone(), 50.0),
        query_p95_us: pct(&mut samples.clone(), 95.0),
        query_p99_us: pct(&mut samples, 99.0),
    })
}
