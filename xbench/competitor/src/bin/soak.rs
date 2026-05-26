//! Long-running stability soak for spg-server.
//!
//! Spawns spg-server, primes a `users` table + a `vecs` HNSW index,
//! then runs a mixed workload (60% indexed SELECT, 30% INSERT, 10%
//! HNSW search) for `SOAK_MINUTES` minutes. Every 30 s samples:
//!
//!   * RSS (KiB) of the server process
//!   * per-op p50 latency over the last sampling window
//!   * cumulative op counts
//!
//! Produces a markdown report at the end. Pass `--minutes N` to
//! override the default (15 minutes); a CI run might use 5, a
//! pre-release run 60+.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin soak [--minutes N]`

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
use std::time::{Duration, Instant};

const SPG_SERVER_ADDR: &str = "127.0.0.1:25549";
const DEFAULT_MINUTES: u64 = 15;
const SAMPLE_INTERVAL_SECS: u64 = 30;
const SEED_ROWS: i32 = 5_000;
const VECTORS: usize = 5_000;
const DIM: usize = 128;
const NEXT_INSERT_ID_START: i32 = 10_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut minutes = DEFAULT_MINUTES;
    let mut mode = "mixed"; // "mixed" or "readonly"
    while let Some(a) = args.next() {
        match a.as_str() {
            "--minutes" => {
                if let Some(v) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    minutes = v;
                }
            }
            "--readonly" => mode = "readonly",
            _ => {}
        }
    }
    let duration = Duration::from_secs(minutes * 60);

    let mut child = spawn_spg_server()?;
    let pid = child.id();
    let result = run_soak(pid, duration, mode == "readonly");
    let _ = child.kill();
    let _ = child.wait();
    let report = result?;

    print_report(&report, minutes, mode);
    Ok(())
}

struct Sample {
    elapsed_sec: u64,
    rss_kib: i64,
    select_p50_us: f64,
    insert_p50_us: f64,
    knn_p50_us: f64,
    cumulative_ops: u64,
}

fn print_report(samples: &[Sample], minutes: u64, mode: &str) {
    let title = if mode == "readonly" {
        format!(
            "{}-min READ-ONLY soak — 60% indexed SELECT, 40% HNSW kNN (no writes)",
            minutes
        )
    } else {
        format!(
            "{}-min MIXED soak — 60% indexed SELECT, 30% INSERT, 10% HNSW kNN",
            minutes
        )
    };
    println!("# {title}");
    println!(
        "# samples every {} s; values are window medians",
        SAMPLE_INTERVAL_SECS
    );
    println!();
    println!("|    t (s) |  RSS KiB |  SEL µs |  INS µs |  kNN µs |  cumulative ops |");
    println!("|---------:|---------:|--------:|--------:|--------:|----------------:|");
    for s in samples {
        println!(
            "| {:>8} | {:>8} | {:>7.1} | {:>7.1} | {:>7.1} | {:>15} |",
            s.elapsed_sec,
            s.rss_kib,
            s.select_p50_us,
            s.insert_p50_us,
            s.knn_p50_us,
            s.cumulative_ops,
        );
    }
    if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
        let rss_drift_pct = if first.rss_kib > 0 {
            ((last.rss_kib - first.rss_kib) as f64) / (first.rss_kib as f64) * 100.0
        } else {
            0.0
        };
        let select_drift_pct = if first.select_p50_us > 0.0 {
            (last.select_p50_us - first.select_p50_us) / first.select_p50_us * 100.0
        } else {
            0.0
        };
        println!();
        println!(
            "## verdict\n\n- RSS drift first→last: **{:+.1}%** (acceptable: < ±20%)",
            rss_drift_pct
        );
        println!(
            "- SELECT p50 drift: **{:+.1}%** (acceptable: < ±20%)",
            select_drift_pct
        );
        println!("- total ops: **{}**", last.cumulative_ops);
    }
}

fn run_soak(
    pid: u32,
    duration: Duration,
    readonly: bool,
) -> Result<Vec<Sample>, Box<dyn std::error::Error>> {
    let stream = TcpStream::connect(SPG_SERVER_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);

    // Seed the schema.
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
        let sql = format!("INSERT INTO users VALUES ({i}, 'u-{i}')");
        round_trip(&mut writer, &mut reader, &sql)?;
    }
    round_trip(
        &mut writer,
        &mut reader,
        &format!(
            "CREATE TABLE vecs (id INT NOT NULL, v VECTOR({}) NOT NULL)",
            DIM
        ),
    )?;
    for i in 0..VECTORS {
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
    eprintln!(
        "seed done: {} rows + {} dim-{} vectors. starting {}-min soak…",
        SEED_ROWS,
        VECTORS,
        DIM,
        duration.as_secs() / 60
    );

    let start = Instant::now();
    let mut samples = Vec::new();
    let mut next_sample = SAMPLE_INTERVAL_SECS;
    let mut next_insert_id: i32 = NEXT_INSERT_ID_START;
    let mut total_ops: u64 = 0;
    let mut window_selects: Vec<u64> = Vec::new();
    let mut window_inserts: Vec<u64> = Vec::new();
    let mut window_knns: Vec<u64> = Vec::new();

    while start.elapsed() < duration {
        let pick = total_ops % 10;
        let t = Instant::now();
        if readonly {
            // Readonly soak: 60% SELECT, 40% HNSW (no INSERTs at all
            // — pure leak detector since data volume is constant).
            if pick < 6 {
                let id = (total_ops as i32 % SEED_ROWS) + 1;
                round_trip(
                    &mut writer,
                    &mut reader,
                    &format!("SELECT id, name FROM users WHERE id = {id}"),
                )?;
                window_selects.push(t.elapsed().as_nanos() as u64);
            } else {
                let mut sql = String::from("SELECT id FROM vecs ORDER BY v <-> [");
                for d in 0..DIM {
                    if d > 0 {
                        sql.push(',');
                    }
                    let f = ((total_ops as usize * 13 + d * 5) % 997) as f32 * 0.001;
                    sql.push_str(&format!("{:.4}", f));
                }
                sql.push_str("] LIMIT 10");
                round_trip(&mut writer, &mut reader, &sql)?;
                window_knns.push(t.elapsed().as_nanos() as u64);
            }
        } else {
            match pick {
                0..=5 => {
                    let id = (total_ops as i32 % SEED_ROWS) + 1;
                    round_trip(
                        &mut writer,
                        &mut reader,
                        &format!("SELECT id, name FROM users WHERE id = {id}"),
                    )?;
                    window_selects.push(t.elapsed().as_nanos() as u64);
                }
                6..=8 => {
                    next_insert_id += 1;
                    let id = next_insert_id;
                    round_trip(
                        &mut writer,
                        &mut reader,
                        &format!("INSERT INTO users VALUES ({id}, 'u-{id}')"),
                    )?;
                    window_inserts.push(t.elapsed().as_nanos() as u64);
                }
                _ => {
                    let mut sql = String::from("SELECT id FROM vecs ORDER BY v <-> [");
                    for d in 0..DIM {
                        if d > 0 {
                            sql.push(',');
                        }
                        let f = ((total_ops as usize * 13 + d * 5) % 997) as f32 * 0.001;
                        sql.push_str(&format!("{:.4}", f));
                    }
                    sql.push_str("] LIMIT 10");
                    round_trip(&mut writer, &mut reader, &sql)?;
                    window_knns.push(t.elapsed().as_nanos() as u64);
                }
            }
        }
        total_ops += 1;

        let elapsed_sec = start.elapsed().as_secs();
        if elapsed_sec >= next_sample {
            let s = Sample {
                elapsed_sec,
                rss_kib: rss_kib(pid),
                select_p50_us: median_us(&mut window_selects),
                insert_p50_us: median_us(&mut window_inserts),
                knn_p50_us: median_us(&mut window_knns),
                cumulative_ops: total_ops,
            };
            eprintln!(
                "  t={}s rss={}KiB ops={}  sel={:.1}µs ins={:.1}µs knn={:.1}µs",
                s.elapsed_sec,
                s.rss_kib,
                s.cumulative_ops,
                s.select_p50_us,
                s.insert_p50_us,
                s.knn_p50_us
            );
            samples.push(s);
            window_selects.clear();
            window_inserts.clear();
            window_knns.clear();
            next_sample += SAMPLE_INTERVAL_SECS;
        }
    }
    Ok(samples)
}

fn median_us(samples: &mut [u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_unstable();
    samples[samples.len() / 2] as f64 / 1000.0
}

fn rss_kib(pid: u32) -> i64 {
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
