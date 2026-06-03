//! v6.9.0 — Concurrency bench.
//!
//! Drives N concurrent native-wire clients against a server and
//! measures throughput + p99 latency for two workload shapes:
//!
//!   1. **SELECT-only.** All N clients run `SELECT id FROM t
//!      WHERE id = <random>` in a tight loop. Stresses the
//!      engine read-lock path; representative of read-heavy
//!      OLTP traffic.
//!   2. **Mixed (75% SELECT / 25% INSERT).** A 4:1 mix
//!      modelled on PG's TPC-B-style mixed workload. Stresses
//!      the engine write-lock path interleaved with reads.
//!
//! Output (per N ∈ {8, 16, 32}):
//!   - aggregate ops/sec across all clients
//!   - p99 latency on the SELECT side
//!
//! Marked `#[ignore]` — runtime is multi-second and CPU-bound,
//! so it doesn't belong in the default sweep. Run with
//!
//! ```sh
//! cargo test -p spg-server --test perf_concurrency --release -- --ignored --nocapture
//! ```
//!
//! The output is consumed by the v6.9.1 ship-rollup decision
//! between (a) entering Choice A — parallel prepare under
//! `engine.read()` + install-phase OCC retry — and (b) deferring
//! Choice A to a future v6.x revisit.

#![allow(clippy::uninlined_format_args, unsafe_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_error_response};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WARMUP_ROWS: i64 = 5_000;
const PER_CLIENT_OPS_SELECT: u64 = 2_000;
const PER_CLIENT_OPS_MIXED: u64 = 500;
const CLIENT_COUNTS: &[usize] = &[8, 16, 32];

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-perf-conc-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_until_cc(s: &mut TcpStream) -> Result<(), String> {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).map_err(|e| format!("bad op: {e:?}"))?;
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).map_err(|e| e.to_string())?;
        }
        match op {
            Op::CommandComplete => return Ok(()),
            Op::DataRow | Op::DataRowBatch | Op::RowDescription => continue,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                return Err(parse_error_response(&f)
                    .unwrap_or("<undecodable>")
                    .to_string());
            }
            _ => continue,
        }
    }
}

fn drain_until_cc_strict(s: &mut TcpStream, sql: &str) {
    if let Err(e) = drain_until_cc(s) {
        panic!("SQL failed: {sql:?} → {e}");
    }
}

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc_strict(s, sql);
}

fn run_select_only(addr: &str, n_clients: usize) -> (f64, Duration) {
    let mut handles = Vec::with_capacity(n_clients);
    let total_ops = (n_clients as u64) * PER_CLIENT_OPS_SELECT;
    let start = Instant::now();
    let addr = Arc::new(addr.to_string());
    for client_id in 0..n_clients {
        let addr = Arc::clone(&addr);
        handles.push(std::thread::spawn(move || -> Vec<Duration> {
            let mut s = TcpStream::connect(addr.as_str()).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let mut latencies: Vec<Duration> = Vec::with_capacity(PER_CLIENT_OPS_SELECT as usize);
            // Cheap deterministic key sequence (no PRNG; tests
            // can't use Rust's default rng — the project bans
            // external deps).
            let mut key = (client_id as i64).wrapping_mul(31) % WARMUP_ROWS;
            for _ in 0..PER_CLIENT_OPS_SELECT {
                let sql = format!("SELECT id FROM t WHERE id = {key}");
                let t0 = Instant::now();
                send_query(&mut s, &sql);
                drain_until_cc(&mut s).unwrap();
                latencies.push(t0.elapsed());
                key = (key + 17) % WARMUP_ROWS;
            }
            latencies
        }));
    }
    let mut all_latencies: Vec<Duration> = Vec::new();
    for h in handles {
        all_latencies.extend(h.join().unwrap());
    }
    let elapsed = start.elapsed();
    let ops_per_sec = (total_ops as f64) / elapsed.as_secs_f64();
    all_latencies.sort();
    let p99_idx = (all_latencies.len() as f64 * 0.99) as usize;
    let p99 = all_latencies
        .get(p99_idx.min(all_latencies.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default();
    (ops_per_sec, p99)
}

fn run_mixed(addr: &str, n_clients: usize, seed_id: u64) -> (f64, Duration) {
    let mut handles = Vec::with_capacity(n_clients);
    let total_ops = (n_clients as u64) * PER_CLIENT_OPS_MIXED;
    let start = Instant::now();
    let addr = Arc::new(addr.to_string());
    for client_id in 0..n_clients {
        let addr = Arc::clone(&addr);
        let base_id = seed_id + (client_id as u64) * 10_000;
        handles.push(std::thread::spawn(move || -> Vec<Duration> {
            let mut s = TcpStream::connect(addr.as_str()).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let mut latencies: Vec<Duration> = Vec::with_capacity(PER_CLIENT_OPS_MIXED as usize);
            let mut key = (client_id as i64).wrapping_mul(31) % WARMUP_ROWS;
            let mut insert_offset: u64 = 0;
            for op_idx in 0..PER_CLIENT_OPS_MIXED {
                let t0 = Instant::now();
                if op_idx % 4 == 3 {
                    // 25% INSERT — fresh PK so no duplicate-key
                    // contention.
                    let pk = base_id + insert_offset + WARMUP_ROWS as u64;
                    insert_offset += 1;
                    let sql = format!("INSERT INTO t VALUES ({pk}, 'r-{pk}')");
                    send_query(&mut s, &sql);
                    drain_until_cc(&mut s).unwrap();
                } else {
                    let sql = format!("SELECT id FROM t WHERE id = {key}");
                    send_query(&mut s, &sql);
                    drain_until_cc(&mut s).unwrap();
                    key = (key + 17) % WARMUP_ROWS;
                }
                latencies.push(t0.elapsed());
            }
            latencies
        }));
    }
    let mut all_latencies: Vec<Duration> = Vec::new();
    for h in handles {
        all_latencies.extend(h.join().unwrap());
    }
    let elapsed = start.elapsed();
    let ops_per_sec = (total_ops as f64) / elapsed.as_secs_f64();
    all_latencies.sort();
    let p99_idx = (all_latencies.len() as f64 * 0.99) as usize;
    let p99 = all_latencies
        .get(p99_idx.min(all_latencies.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default();
    (ops_per_sec, p99)
}

#[test]
#[ignore]
fn concurrency_bench() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .arg("-")
        .arg_path(&wal)
        .env("SPG_FREEZER_DISABLE", "1")
        .spawn();
    let _guard = common::ChildGuard(raw);
    // Warm the catalog: one table + index + WARMUP_ROWS rows.
    {
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)");
        exec_native(&mut s, "CREATE INDEX by_id ON t (id)");
        const BATCH: i64 = 256;
        let mut i: i64 = 0;
        while i < WARMUP_ROWS {
            let upper = (i + BATCH).min(WARMUP_ROWS);
            let mut sql = String::with_capacity(64 * BATCH as usize);
            sql.push_str("INSERT INTO t VALUES ");
            let mut first = true;
            for k in i..upper {
                if !first {
                    sql.push(',');
                }
                first = false;
                sql.push_str(&format!("({k}, 'r-{k}')"));
            }
            exec_native(&mut s, &sql);
            i = upper;
        }
    }

    println!("=== v6.9.0 concurrency bench ===");
    println!(
        "warmup rows={WARMUP_ROWS}, select-ops/client={PER_CLIENT_OPS_SELECT}, mixed-ops/client={PER_CLIENT_OPS_MIXED}"
    );

    println!("\n[SELECT-only]");
    for &n in CLIENT_COUNTS {
        let (ops, p99) = run_select_only(&addrs.native, n);
        println!(
            "  clients={n}  → {:>9.0} ops/sec   p99={:?}",
            ops, p99
        );
    }

    let mut seed: u64 = 1_000_000;
    println!("\n[Mixed 75% SELECT / 25% INSERT]");
    for &n in CLIENT_COUNTS {
        let (ops, p99) = run_mixed(&addrs.native, n, seed);
        println!(
            "  clients={n}  → {:>9.0} ops/sec   p99={:?}",
            ops, p99
        );
        seed += 10_000_000;
    }
    println!();
}
