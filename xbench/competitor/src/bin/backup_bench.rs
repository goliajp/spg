//! v4.25 backup bench — full / incremental bandwidth + restore round-trip.
//!
//! Measures four prod numbers:
//!
//! 1. **Full backup bandwidth** — how fast `BACKUP TO '<path>'`
//!    streams a 100K-row snapshot to disk.
//! 2. **Incremental bandwidth** — bytes/sec when shipping the WAL
//!    tail after a 10K-row delta.
//! 3. **Restore round-trip** — fresh server starts from a captured
//!    full bundle + incremental and reports the same row count.
//! 4. **PITR overhead** — server startup with `SPG_REPLAY_UPTO` set
//!    vs unset, on a populated WAL.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin backup_bench`

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SPG_ADDR: &str = "127.0.0.1:25671";
const RESTORE_ADDR: &str = "127.0.0.1:25672";
const BASELINE_ROWS: usize = 100_000;
const INCREMENTAL_ROWS: usize = 10_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tmpdir();
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let full = dir.join("full.bkp");
    let incr = dir.join("incr.bkp");

    println!("# v4.25 backup bench");
    println!();

    // Spawn primary with WAL.
    let mut server = spawn_server(SPG_ADDR, &db, &wal)?;
    wait_for_listener(SPG_ADDR)?;

    // Seed BASELINE_ROWS via 100-row VALUES batches for speed.
    seed_baseline(SPG_ADDR, BASELINE_ROWS)?;
    let wal_size_after_seed = std::fs::metadata(&wal)?.len();
    println!("## Seeded {BASELINE_ROWS} rows");
    println!();
    println!("- WAL size after seed : {} KiB", wal_size_after_seed / 1024);
    println!();

    // ---- Part 1: full backup bandwidth ----
    let t = Instant::now();
    let full_pos = backup_full(SPG_ADDR, &full)?;
    let elapsed = t.elapsed().as_secs_f64();
    let bundle_size = std::fs::metadata(&full)?.len();
    println!("## Full backup");
    println!();
    println!("- bundle path         : {}", full.display());
    println!("- bundle size         : {} KiB", bundle_size / 1024);
    println!("- wal_pos captured    : {full_pos}");
    println!("- elapsed             : {:.0} ms", elapsed * 1000.0);
    println!(
        "- bandwidth           : {:.1} MiB/s",
        (bundle_size as f64) / elapsed / (1024.0 * 1024.0)
    );
    println!();

    // ---- Part 2: incremental backup ----
    seed_more(SPG_ADDR, BASELINE_ROWS, INCREMENTAL_ROWS)?;
    let t = Instant::now();
    let incr_pos = backup_incremental(SPG_ADDR, &incr, full_pos)?;
    let elapsed = t.elapsed().as_secs_f64();
    let bundle_size = std::fs::metadata(&incr)?.len();
    println!("## Incremental backup ({INCREMENTAL_ROWS} new rows since SINCE={full_pos})");
    println!();
    println!("- bundle size         : {} KiB", bundle_size / 1024);
    println!("- wal_pos captured    : {incr_pos}");
    println!("- elapsed             : {:.0} ms", elapsed * 1000.0);
    println!(
        "- bandwidth           : {:.1} MiB/s",
        (bundle_size as f64) / elapsed / (1024.0 * 1024.0)
    );
    println!();

    // Tear down primary before restore.
    let _ = server.kill();
    let _ = server.wait();
    std::thread::sleep(Duration::from_millis(200));

    // ---- Part 3: restore round-trip ----
    let rec_dir = tmpdir();
    let rec_db = rec_dir.join("rec.db");
    let rec_wal = rec_dir.join("rec.wal");
    let t = Instant::now();
    apply_bundle_to(&rec_db, &rec_wal, &full)?;
    apply_bundle_to(&rec_db, &rec_wal, &incr)?;
    let apply_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let mut restored = spawn_server(RESTORE_ADDR, &rec_db, &rec_wal)?;
    wait_for_listener(RESTORE_ADDR)?;
    let startup_ms = t.elapsed().as_secs_f64() * 1000.0;
    let count = count_rows(RESTORE_ADDR, "bench")?;
    println!("## Restore round-trip");
    println!();
    println!("- bundle apply time   : {apply_ms:.0} ms");
    println!("- server startup time : {startup_ms:.0} ms");
    println!(
        "- restored row count  : {count} (expected {})",
        BASELINE_ROWS + INCREMENTAL_ROWS
    );
    assert_eq!(count as usize, BASELINE_ROWS + INCREMENTAL_ROWS);
    let _ = restored.kill();
    let _ = restored.wait();
    std::thread::sleep(Duration::from_millis(200));
    println!();

    // ---- Part 4: PITR demonstration ----
    // SPG_REPLAY_UPTO truncates LOCAL wal replay at a byte offset.
    // After Part 3 the rec_wal contains the *incremental* WAL slice
    // (the full bundle shipped wal_len=0 by design — its snapshot
    // already encodes every committed write up to the pivot).
    // So the local rec_wal is a {INCREMENTAL_ROWS}-row tail.
    //
    // Compare:
    //   (a) startup with full WAL replay   → 100K + 10K rows
    //   (b) startup with SPG_REPLAY_UPTO=0 → snapshot only, 100K rows
    //       (operator's "roll back to the full-backup pivot" mode)
    let t = Instant::now();
    let mut s1 = spawn_server(RESTORE_ADDR, &rec_db, &rec_wal)?;
    wait_for_listener(RESTORE_ADDR)?;
    let baseline_ms = t.elapsed().as_secs_f64() * 1000.0;
    let n1 = count_rows(RESTORE_ADDR, "bench")?;
    let _ = s1.kill();
    let _ = s1.wait();
    std::thread::sleep(Duration::from_millis(200));

    let t = Instant::now();
    let mut s2 =
        spawn_server_with_env(RESTORE_ADDR, &rec_db, &rec_wal, &[("SPG_REPLAY_UPTO", "0")])?;
    wait_for_listener(RESTORE_ADDR)?;
    let pitr_ms = t.elapsed().as_secs_f64() * 1000.0;
    let pitr_count = count_rows(RESTORE_ADDR, "bench")?;
    let _ = s2.kill();
    let _ = s2.wait();

    println!("## PITR (SPG_REPLAY_UPTO truncation)");
    println!();
    println!("- full WAL replay startup        : {baseline_ms:.0} ms (rows={n1})");
    println!("- SPG_REPLAY_UPTO=0 startup      : {pitr_ms:.0} ms (rows={pitr_count})");
    println!(
        "                                    expected baseline {} + incr {} = {}, then truncated to {}",
        BASELINE_ROWS,
        INCREMENTAL_ROWS,
        BASELINE_ROWS + INCREMENTAL_ROWS,
        BASELINE_ROWS
    );
    Ok(())
}

fn seed_baseline(addr: &str, n: usize) -> Result<(), Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(16 * 1024, s);
    round_trip(
        &mut w,
        &mut r,
        "CREATE TABLE bench (id INT NOT NULL, v INT NOT NULL)",
    )?;
    let batch = 100;
    let mut i = 0;
    while i < n {
        let upto = (i + batch).min(n);
        let mut sql = String::from("INSERT INTO bench VALUES ");
        let mut first = true;
        for k in i..upto {
            if !first {
                sql.push(',');
            }
            first = false;
            sql.push_str(&format!("({k}, {k})"));
        }
        round_trip(&mut w, &mut r, &sql)?;
        i = upto;
    }
    Ok(())
}

fn seed_more(addr: &str, start: usize, count: usize) -> Result<(), Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(16 * 1024, s);
    let batch = 100;
    let mut i = start;
    let end = start + count;
    while i < end {
        let upto = (i + batch).min(end);
        let mut sql = String::from("INSERT INTO bench VALUES ");
        let mut first = true;
        for k in i..upto {
            if !first {
                sql.push(',');
            }
            first = false;
            sql.push_str(&format!("({k}, {k})"));
        }
        round_trip(&mut w, &mut r, &sql)?;
        i = upto;
    }
    Ok(())
}

fn backup_full(addr: &str, path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(8 * 1024, s);
    let sql = format!("BACKUP TO '{}'", path.display());
    Ok(round_trip_count(&mut w, &mut r, &sql)?)
}

fn backup_incremental(
    addr: &str,
    path: &Path,
    since: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(8 * 1024, s);
    let sql = format!("BACKUP TO '{}' INCREMENTAL SINCE {since}", path.display());
    Ok(round_trip_count(&mut w, &mut r, &sql)?)
}

fn count_rows(addr: &str, table: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::with_capacity(8 * 1024, s);
    Ok(select_int(
        &mut w,
        &mut r,
        &format!("SELECT count(*) FROM {table}"),
    )?)
}

fn apply_bundle_to(
    dest_db: &Path,
    dest_wal: &Path,
    bundle: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(bundle)?;
    if &bytes[..8] != b"SPGBKUP\x01" {
        return Err("bad bundle magic".into());
    }
    let snap_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap()) as usize;
    let snap_end = 33 + snap_len;
    let wal_len =
        u64::from_le_bytes(bytes[snap_end + 8..snap_end + 16].try_into().unwrap()) as usize;
    let wal_start = snap_end + 16;
    let wal_slice = &bytes[wal_start..wal_start + wal_len];
    if snap_len > 0 {
        std::fs::write(dest_db, &bytes[33..33 + snap_len])?;
        std::fs::write(dest_wal, wal_slice)?;
    } else {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dest_wal)?;
        f.write_all(wal_slice)?;
        f.sync_data()?;
    }
    Ok(())
}

fn round_trip<W: Write, R: Read>(w: &mut W, r: &mut BufReader<R>, sql: &str) -> Result<(), String> {
    use spg_wire::{Op, build_query, encode};
    let mut out = Vec::with_capacity(64);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    w.write_all(&out).map_err(|e| format!("write: {e}"))?;
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        r.read_exact(&mut hdr).map_err(|e| format!("hdr: {e}"))?;
        let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).map_err(|e| format!("op {e:?}"))?;
        let mut payload = vec![0u8; plen];
        if plen > 0 {
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

fn round_trip_count<W: Write, R: Read>(
    w: &mut W,
    r: &mut BufReader<R>,
    sql: &str,
) -> Result<u64, String> {
    use spg_wire::{Op, build_query, encode, parse_command_complete};
    let mut out = Vec::with_capacity(64);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    w.write_all(&out).map_err(|e| format!("write: {e}"))?;
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        r.read_exact(&mut hdr).map_err(|e| format!("hdr: {e}"))?;
        let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).map_err(|e| format!("op {e:?}"))?;
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            r.read_exact(&mut payload)
                .map_err(|e| format!("body: {e}"))?;
        }
        let f = spg_wire::Frame { op, payload };
        match op {
            Op::CommandComplete => {
                return parse_command_complete(&f).map_err(|e| format!("cc: {e}"));
            }
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f)
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
        let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).map_err(|e| format!("op {e:?}"))?;
        let mut payload = vec![0u8; plen];
        if plen > 0 {
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
                if let Some(rr) = rows.first() {
                    val = wire_to_i64(&rr[0]);
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

fn spawn_server(addr: &str, db: &Path, wal: &Path) -> std::io::Result<Child> {
    spawn_server_with_env(addr, db, wal, &[])
}

fn spawn_server_with_env(
    addr: &str,
    db: &Path,
    wal: &Path,
    env: &[(&str, &str)],
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
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn()
}

fn wait_for_listener(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let p = std::env::temp_dir().join(format!("spg-backup-bench-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
