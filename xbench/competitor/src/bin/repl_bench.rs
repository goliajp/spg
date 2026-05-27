//! v4.24 replication throughput bench.
//!
//! Spins up a primary (with WAL + SPG_REPL_ADDR) and a follower
//! (with SPG_FOLLOW_OF), then measures three things prod operators
//! actually care about:
//!
//! 1. **Snapshot bootstrap time** — how long from `SPG_FOLLOW_OF`
//!    set to follower-sees-baseline rows.
//! 2. **Write throughput cost of attaching a follower** — INSERT
//!    rate on the primary with and without the follower wired up.
//! 3. **Replication lag** — distribution of (primary commit time →
//!    follower visible time) over N writes.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin repl_bench`

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PRIMARY_NATIVE: &str = "127.0.0.1:25661";
const PRIMARY_REPL: &str = "127.0.0.1:25662";
const FOLLOWER_NATIVE: &str = "127.0.0.1:25663";
const BASELINE_ROWS: usize = 1_000;
const WRITE_BURST_ROWS: usize = 2_000;
const LAG_PROBE_ROWS: usize = 200;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tmpdir();
    let p_db = dir.join("p.db");
    let p_wal = dir.join("p.wal");
    let f_db = dir.join("f.db");
    let f_wal = dir.join("f.wal");

    println!("# v4.24 replication bench");
    println!();

    // ---- Part 1: primary-only INSERT throughput (baseline) ----
    println!("## INSERT throughput on primary");
    println!();
    let mut primary: Child = spawn_server(PRIMARY_NATIVE, &p_db, &p_wal, &[])?;
    wait_for_listener(PRIMARY_NATIVE)?;
    seed_table(PRIMARY_NATIVE)?;
    let solo_rate = measure_insert_rate(PRIMARY_NATIVE, WRITE_BURST_ROWS)?;
    println!(
        "- solo (no follower)     : {WRITE_BURST_ROWS} rows in {:.2}ms = {:.0} rows/s",
        solo_rate.0, solo_rate.1
    );
    // tear down so we get a clean WAL position counter for Part 2/3.
    // `Child::drop` doesn't kill — must wait + kill explicitly.
    let _ = primary.kill();
    let _ = primary.wait();
    let _ = std::fs::remove_file(&p_db);
    let _ = std::fs::remove_file(&p_wal);
    // Wait briefly for the OS to release the port too.
    std::thread::sleep(Duration::from_millis(200));

    // ---- Part 2: snapshot bootstrap latency ----
    println!();
    println!("## Snapshot bootstrap latency (follower → caught up to N seed rows)");
    println!();
    primary = spawn_server(
        PRIMARY_NATIVE,
        &p_db,
        &p_wal,
        &[("SPG_REPL_ADDR", PRIMARY_REPL)],
    )?;
    wait_for_listener(PRIMARY_NATIVE)?;
    seed_table(PRIMARY_NATIVE)?;
    fill_table(PRIMARY_NATIVE, BASELINE_ROWS)?;

    let bootstrap_started = Instant::now();
    let follower = spawn_server(
        FOLLOWER_NATIVE,
        &f_db,
        &f_wal,
        &[("SPG_FOLLOW_OF", PRIMARY_REPL)],
    )?;
    wait_for_listener(FOLLOWER_NATIVE)?;
    // Poll follower until it sees the baseline.
    let bootstrap_ms = poll_until_count(FOLLOWER_NATIVE, BASELINE_ROWS as i64, 30_000)?;
    println!(
        "- baseline {BASELINE_ROWS} rows visible on follower in {:.0}ms (wall = {:.0}ms)",
        bootstrap_ms,
        bootstrap_started.elapsed().as_secs_f64() * 1000.0
    );

    // ---- Part 3: write throughput with follower attached ----
    println!();
    println!("## INSERT throughput with follower attached");
    println!();
    let attached_rate = measure_insert_rate(PRIMARY_NATIVE, WRITE_BURST_ROWS)?;
    let cost_pct = ((solo_rate.1 - attached_rate.1) / solo_rate.1) * 100.0;
    println!(
        "- with follower          : {WRITE_BURST_ROWS} rows in {:.2}ms = {:.0} rows/s",
        attached_rate.0, attached_rate.1
    );
    println!("- attach cost vs solo    : {cost_pct:+.1}% throughput");

    // ---- Part 4: replication lag distribution ----
    println!();
    println!("## Replication lag (primary commit → follower visible)");
    println!();
    let lag_samples = measure_lag(PRIMARY_NATIVE, FOLLOWER_NATIVE, LAG_PROBE_ROWS)?;
    let mut sorted = lag_samples.clone();
    sorted.sort_unstable();
    let p50 = pct(&sorted, 0.50);
    let p95 = pct(&sorted, 0.95);
    let p99 = pct(&sorted, 0.99);
    let max = *sorted.last().unwrap_or(&0);
    println!("- samples : {LAG_PROBE_ROWS}");
    println!("- p50     : {p50} µs");
    println!("- p95     : {p95} µs");
    println!("- p99     : {p99} µs");
    println!("- max     : {max} µs");

    let mut follower = follower;
    let _ = follower.kill();
    let _ = follower.wait();
    let _ = primary.kill();
    let _ = primary.wait();
    Ok(())
}

fn pct(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64) * q).floor() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn measure_insert_rate(addr: &str, n: usize) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut writer = s.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, s);
    let start = Instant::now();
    for i in 0..n {
        // Use unique ids so re-runs don't collide.
        let id = i + 10_000_000;
        round_trip(
            &mut writer,
            &mut reader,
            &format!("INSERT INTO bench VALUES ({id}, {})", id * 2),
        )?;
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let rate = (n as f64) / start.elapsed().as_secs_f64();
    Ok((elapsed_ms, rate))
}

fn measure_lag(
    primary: &str,
    follower: &str,
    n: usize,
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let ps = TcpStream::connect(primary)?;
    ps.set_nodelay(true)?;
    let mut pw = ps.try_clone()?;
    let mut pr = BufReader::with_capacity(16 * 1024, ps);

    let fs = TcpStream::connect(follower)?;
    fs.set_nodelay(true)?;
    let mut fw = fs.try_clone()?;
    let mut fr = BufReader::with_capacity(16 * 1024, fs);

    let mut samples = Vec::with_capacity(n);
    let id_base = 20_000_000;
    for i in 0..n {
        let id = id_base + i;
        let t0 = Instant::now();
        round_trip(
            &mut pw,
            &mut pr,
            &format!("INSERT INTO bench VALUES ({id}, {id})"),
        )?;
        // Poll follower for this specific row.
        loop {
            let sql = format!("SELECT count(*) FROM bench WHERE id = {id}");
            if select_int(&mut fw, &mut fr, &sql)? == 1 {
                break;
            }
            if t0.elapsed() > Duration::from_secs(5) {
                return Err(format!("lag probe row {id} never reached follower").into());
            }
        }
        samples.push(t0.elapsed().as_micros() as u64);
    }
    Ok(samples)
}

fn seed_table(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(8 * 1024, s);
    round_trip(
        &mut w,
        &mut r,
        "CREATE TABLE bench (id INT NOT NULL, v INT NOT NULL)",
    )?;
    Ok(())
}

fn fill_table(addr: &str, n: usize) -> Result<(), Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(16 * 1024, s);
    for i in 0..n {
        round_trip(
            &mut w,
            &mut r,
            &format!("INSERT INTO bench VALUES ({i}, 0)"),
        )?;
    }
    Ok(())
}

fn poll_until_count(
    addr: &str,
    expected: i64,
    timeout_ms: u64,
) -> Result<f64, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(16 * 1024, s);
    loop {
        if started.elapsed() > Duration::from_millis(timeout_ms) {
            return Err(
                format!("follower never reached count={expected} within {timeout_ms}ms").into(),
            );
        }
        match select_int(&mut w, &mut r, "SELECT count(*) FROM bench") {
            Ok(n) if n == expected => return Ok(started.elapsed().as_secs_f64() * 1000.0),
            Ok(_) | Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn round_trip<W: Write, R: Read>(w: &mut W, r: &mut BufReader<R>, sql: &str) -> Result<(), String> {
    use spg_wire::{Op, build_query, encode};
    let mut out = Vec::with_capacity(64);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    w.write_all(&out).map_err(|e| format!("write: {e}"))?;
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        r.read_exact(&mut hdr).map_err(|e| format!("hdr: {e}"))?;
        let payload_len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).map_err(|e| format!("bad op {e:?}"))?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            r.read_exact(&mut payload)
                .map_err(|e| format!("body: {e}"))?;
        }
        match op {
            Op::CommandComplete => return Ok(()),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&spg_wire::Frame { op, payload })
                    .map_or_else(|_| "<undecodable>".into(), str::to_owned);
                return Err(format!("{sql:?} -> {msg}"));
            }
            _ => {}
        }
    }
}

fn select_int<W: Write, R: Read>(
    w: &mut W,
    r: &mut BufReader<R>,
    sql: &str,
) -> Result<i64, String> {
    use spg_wire::{Op, build_query, encode, parse_data_row, parse_data_row_batch};
    let mut out = Vec::with_capacity(64);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    w.write_all(&out).map_err(|e| format!("write: {e}"))?;
    let mut val: i64 = -1;
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        r.read_exact(&mut hdr).map_err(|e| format!("hdr: {e}"))?;
        let payload_len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).map_err(|e| format!("bad op {e:?}"))?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            r.read_exact(&mut payload)
                .map_err(|e| format!("body: {e}"))?;
        }
        let f = spg_wire::Frame { op, payload };
        match op {
            Op::DataRow => {
                let row = parse_data_row(&f).map_err(|e| format!("dr: {e}"))?;
                val = wire_to_i64(&row[0]);
            }
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).map_err(|e| format!("drb: {e}"))?;
                if let Some(r) = rows.first() {
                    val = wire_to_i64(&r[0]);
                }
            }
            Op::CommandComplete => return Ok(val),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f)
                    .map_or_else(|_| "<undecodable>".into(), str::to_owned);
                return Err(format!("{sql:?} -> {msg}"));
            }
            _ => {}
        }
    }
}

fn wire_to_i64(v: &spg_wire::WireValue) -> i64 {
    use spg_wire::WireValue;
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap_or(0),
        _ => 0,
    }
}

fn spawn_server(
    addr: &str,
    db: &Path,
    wal: &Path,
    extra_env: &[(&str, &str)],
) -> std::io::Result<Child> {
    let _ = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status();
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let mut cmd = Command::new(&bin);
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn()
}

fn wait_for_listener(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("{addr} never came up").into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-repl-bench-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
