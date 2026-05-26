//! RSS-at-workload baseline for spg-server.
//!
//! Spawns the binary, samples Resident Set Size (KiB) at:
//!   * idle (just bound, no client)
//!   * after 10K-row INSERT
//!   * after 100K-row INSERT
//!   * after HNSW index over 10K dim-128 vectors
//!   * peak observed during workload
//!
//! Plus an SPG-embedded comparison (no server process — measures
//! self-RSS so embedded vs server overhead is visible).
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin memory`

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

const SPG_SERVER_ADDR: &str = "127.0.0.1:25547";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("# RSS at workload (kibibytes resident in physical memory)");
    println!();

    // SPG embedded path — measure the bench process itself.
    let embedded = measure_embedded();
    println!("## spg-embedded (in-process Engine, self-RSS)");
    println!();
    print_rss_table(&embedded);
    println!();

    // SPG server path — measure the spawned child process.
    let server = measure_server()?;
    println!("## spg-server (spawned binary, server-RSS)");
    println!();
    print_rss_table(&server);
    println!();

    Ok(())
}

#[derive(Default, Clone, Debug)]
struct RssReport {
    idle_kib: i64,
    after_10k_rows: i64,
    after_100k_rows: i64,
    after_hnsw_10k_d128: i64,
    peak_kib: i64,
}

fn print_rss_table(r: &RssReport) {
    println!("| stage                         |  RSS KiB  |   RSS MiB |");
    println!("|-------------------------------|----------:|----------:|");
    for (label, v) in [
        ("idle (no workload)", r.idle_kib),
        ("after 10K-row INSERT", r.after_10k_rows),
        ("after 100K-row INSERT", r.after_100k_rows),
        ("after 10K dim-128 HNSW", r.after_hnsw_10k_d128),
        ("peak observed", r.peak_kib),
    ] {
        let mib = (v as f64) / 1024.0;
        println!("| {:<29} | {:>9} | {:>9.1} |", label, v, mib);
    }
}

/// Read RSS (KiB) for `pid` via `ps` (portable across macOS / Linux).
fn rss_kib(pid: u32) -> i64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse::<i64>().unwrap_or(0)
        }
        _ => 0,
    }
}

fn self_rss_kib() -> i64 {
    rss_kib(std::process::id())
}

// ----- SPG embedded -----------------------------------------------------

fn measure_embedded() -> RssReport {
    use spg_engine::Engine;
    let mut report = RssReport::default();
    let mut peak: i64 = 0;
    let sample = |label: &str, val: &mut i64, peak: &mut i64| {
        let v = self_rss_kib();
        *val = v;
        if v > *peak {
            *peak = v;
        }
        eprintln!("  [embedded] {label}: {v} KiB");
    };

    let mut eng = Engine::new();
    eng.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    sample("idle", &mut report.idle_kib, &mut peak);

    let mut id = 1usize;
    while id <= 10_000 {
        // 100-row VALUES batch for speed.
        let mut sql = String::from("INSERT INTO users (id, name) VALUES ");
        let end = (id + 100).min(10_001);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        eng.execute(&sql).unwrap();
        id = end;
    }
    sample(
        "after 10K-row INSERT",
        &mut report.after_10k_rows,
        &mut peak,
    );

    while id <= 100_000 {
        let mut sql = String::from("INSERT INTO users (id, name) VALUES ");
        let end = (id + 100).min(100_001);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        eng.execute(&sql).unwrap();
        id = end;
    }
    sample(
        "after 100K-row INSERT",
        &mut report.after_100k_rows,
        &mut peak,
    );

    // 10K dim-128 vectors.
    eng.execute("CREATE TABLE vecs (id INT NOT NULL, v VECTOR(128) NOT NULL)")
        .unwrap();
    for i in 0..10_000 {
        let mut sql = format!("INSERT INTO vecs VALUES ({i}, [");
        for d in 0..128 {
            if d > 0 {
                sql.push(',');
            }
            let f = ((i + d) as f32) * 0.001;
            sql.push_str(&format!("{:.4}", f));
        }
        sql.push_str("])");
        eng.execute(&sql).unwrap();
    }
    eng.execute("CREATE INDEX vecs_idx ON vecs USING hnsw (v)")
        .unwrap();
    sample(
        "after 10K dim-128 HNSW",
        &mut report.after_hnsw_10k_d128,
        &mut peak,
    );

    report.peak_kib = peak;
    report
}

// ----- SPG server -------------------------------------------------------

fn measure_server() -> Result<RssReport, Box<dyn std::error::Error>> {
    let mut report = RssReport::default();
    let mut child = spawn_spg_server()?;
    let pid = child.id();
    let mut peak: i64 = 0;

    let sample = |label: &str, val: &mut i64, peak: &mut i64| {
        let v = rss_kib(pid);
        *val = v;
        if v > *peak {
            *peak = v;
        }
        eprintln!("  [server] {label}: {v} KiB");
    };

    // Give the server a moment to settle after stderr "listening on".
    std::thread::sleep(Duration::from_millis(100));
    sample("idle", &mut report.idle_kib, &mut peak);

    let stream = TcpStream::connect(SPG_SERVER_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_mins(2)))?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);

    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)",
    )?;

    let mut id = 1usize;
    while id <= 10_000 {
        let mut sql = String::from("INSERT INTO users (id, name) VALUES ");
        let end = (id + 100).min(10_001);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        round_trip(&mut writer, &mut reader, &sql)?;
        id = end;
    }
    sample(
        "after 10K-row INSERT",
        &mut report.after_10k_rows,
        &mut peak,
    );

    while id <= 100_000 {
        let mut sql = String::from("INSERT INTO users (id, name) VALUES ");
        let end = (id + 100).min(100_001);
        for j in id..end {
            if j > id {
                sql.push(',');
            }
            sql.push_str(&format!("({j}, 'u-{j}')"));
        }
        round_trip(&mut writer, &mut reader, &sql)?;
        id = end;
    }
    sample(
        "after 100K-row INSERT",
        &mut report.after_100k_rows,
        &mut peak,
    );

    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE vecs (id INT NOT NULL, v VECTOR(128) NOT NULL)",
    )?;
    for i in 0..10_000 {
        let mut sql = format!("INSERT INTO vecs VALUES ({i}, [");
        for d in 0..128 {
            if d > 0 {
                sql.push(',');
            }
            let f = ((i + d) as f32) * 0.001;
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
    sample(
        "after 10K dim-128 HNSW",
        &mut report.after_hnsw_10k_d128,
        &mut peak,
    );
    report.peak_kib = peak;

    let _ = child.kill();
    let _ = child.wait();
    Ok(report)
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
