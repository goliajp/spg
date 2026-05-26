//! Concurrent SELECT stress + scaling test for spg-server.
//!
//! Spawns N reader threads, each running its own TCP connection to
//! the server. Each thread runs the same indexed SELECT for D
//! seconds. Reports per-thread throughput + aggregate ops/sec.
//!
//! For v4.0 (RwLock + read/write split) the expectation is
//! near-linear scaling for the read-only workload up to physical
//! core count; the v3.x `Mutex<Engine>` shape would have flat-lined
//! at single-thread rate.
//!
//! Run: cargo run --release -p spg-bench-competitor --bin concurrent
//!      [--threads N] [--seconds S]

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const SPG_SERVER_ADDR: &str = "127.0.0.1:25550";
const SEED_ROWS: i32 = 10_000;
const DEFAULT_THREADS: usize = 8;
const DEFAULT_SECONDS: u64 = 10;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (threads, seconds) = parse_args();
    let mut child = spawn_spg_server()?;
    let res = run(threads, seconds);
    let _ = child.kill();
    let _ = child.wait();
    res
}

fn parse_args() -> (usize, u64) {
    let mut threads = DEFAULT_THREADS;
    let mut seconds = DEFAULT_SECONDS;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => {
                if let Some(v) = args.next().and_then(|s| s.parse::<usize>().ok()) {
                    threads = v;
                }
            }
            "--seconds" => {
                if let Some(v) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    seconds = v;
                }
            }
            _ => {}
        }
    }
    (threads, seconds)
}

fn run(threads: usize, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    // Seed the table from a single connection so all reader threads
    // see identical data.
    {
        let stream = TcpStream::connect(SPG_SERVER_ADDR)?;
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
        for i in 1..=SEED_ROWS {
            round_trip(
                &mut writer,
                &mut reader,
                &format!("INSERT INTO users VALUES ({i}, 'u-{i}')"),
            )?;
        }
    }
    eprintln!("seed {SEED_ROWS} rows; spawning {threads} reader threads for {seconds}s…");

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || -> Result<u64, String> {
            let stream = TcpStream::connect(SPG_SERVER_ADDR).map_err(|e| e.to_string())?;
            stream.set_nodelay(true).map_err(|e| e.to_string())?;
            let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
            let mut reader = BufReader::with_capacity(64 * 1024, stream);
            let mut ops: u64 = 0;
            // Each thread targets a slightly different id range so
            // multiple threads' caches stay independent (not all
            // hitting the same hot row).
            let mut id = (tid as i32 * 37) % SEED_ROWS;
            while !stop.load(Ordering::Relaxed) {
                id = (id + 1) % SEED_ROWS;
                let sql = format!("SELECT id, name FROM users WHERE id = {}", id + 1);
                round_trip(&mut writer, &mut reader, &sql)?;
                ops += 1;
            }
            Ok(ops)
        }));
    }

    let start = Instant::now();
    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();

    let mut per_thread: Vec<u64> = Vec::with_capacity(threads);
    for h in handles {
        let ops = h.join().unwrap()?;
        per_thread.push(ops);
    }
    let total: u64 = per_thread.iter().sum();
    print_report(&per_thread, total, elapsed);
    Ok(())
}

fn print_report(per_thread: &[u64], total: u64, elapsed_sec: f64) {
    let n = per_thread.len();
    let mean = total as f64 / n as f64;
    let max = *per_thread.iter().max().unwrap() as f64;
    let min = *per_thread.iter().min().unwrap() as f64;
    let mean_rps = mean / elapsed_sec;
    let total_rps = total as f64 / elapsed_sec;
    let spread_pct = (max - min) / mean * 100.0;

    println!();
    println!(
        "# concurrent SELECT — {} threads × {:.1}s, indexed PK lookup on a {}-row table",
        n, elapsed_sec, SEED_ROWS
    );
    println!();
    println!("| metric                       |         value |");
    println!("|------------------------------|--------------:|");
    println!("| total ops                    | {:>12} |", total);
    println!("| aggregate throughput         | {:>9.0} r/s |", total_rps);
    println!("| mean per-thread throughput   | {:>9.0} r/s |", mean_rps);
    println!(
        "| min per-thread ops           | {:>12} |",
        per_thread.iter().min().unwrap()
    );
    println!(
        "| max per-thread ops           | {:>12} |",
        per_thread.iter().max().unwrap()
    );
    println!("| per-thread spread (%)        | {:>11.1}% |", spread_pct);
    println!();
    println!("## per-thread ops");
    for (i, &v) in per_thread.iter().enumerate() {
        println!("  thread {i:>2}: {v}");
    }
    println!();
    // A well-shared read lock should give ~linear scaling: total_rps
    // grows with thread count, mean per-thread throughput stays
    // roughly flat. If mean falls off a cliff as N grows, contention
    // is the bottleneck.
    println!("## interpretation");
    println!(
        "- if `mean per-thread throughput` stays close across runs at different `--threads`,"
    );
    println!("  the read path scales (RwLock::read in parallel)");
    println!(
        "- if it collapses (e.g. total_rps barely grows past N=1), threads are serialising —"
    );
    println!("  v4.0's RwLock work didn't take effect");
}

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

fn round_trip<W: Write, R: Read>(
    writer: &mut W,
    reader: &mut BufReader<R>,
    sql: &str,
) -> Result<(), String> {
    use spg_wire::{Op, build_query, encode, parse_command_complete, parse_error_response};
    let mut out = Vec::with_capacity(sql.len() + 16);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    writer.write_all(&out).map_err(|e| format!("write: {e}"))?;
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
            Op::CommandComplete => {
                let _ = parse_command_complete(&frame);
                return Ok(());
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
