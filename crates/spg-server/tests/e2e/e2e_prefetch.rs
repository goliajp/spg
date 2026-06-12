//! v6.7.6 ship gate #1 — boot-time prefetch hit metric.
//!
//! V6_7_DESIGN.md L2 names the gate
//! `tests/e2e_prefetch::sequential_scan_triggers_prefetch`.
//! v6.7.6 ships the prefetch pool at the boot path
//! (scan-triggered prefetch is in the L2 spec but carved out per
//! the prefetch.rs module docs — the v6.7 cold tier sits entirely
//! in memory once loaded, so there's no page-cache eviction to
//! refresh between scans). The gate here asserts the metric
//! `spg_cold_prefetch_hits_total` increments by exactly one per
//! manifest-listed cold segment when the server is booted from a
//! db_path that already carries CHECKPOINT'd state.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_error_response};

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-prefetch-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_until_cc(s: &mut TcpStream, sql: &str) {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "SQL failed: {sql:?} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc(s, sql);
}

fn http_get_body(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("http connect");
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let s = String::from_utf8_lossy(&buf).to_string();
    s.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default()
}

fn metric_value(body: &str, name: &str) -> Option<u64> {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{name} ")) {
            if let Ok(v) = rest.trim().parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

/// Row count of `SELECT * FROM spg_stat_segment` = LIVE cold
/// segments per the catalog (tombstoned segments keep disk files
/// but leave the manifest — exactly what the boot prefetch walks).
fn count_stat_segment_rows(s: &mut TcpStream) -> u64 {
    send_query(s, "SELECT * FROM spg_stat_segment");
    let mut total = 0u64;
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::DataRow => total += 1,
            Op::DataRowBatch => {
                let f = spg_wire::Frame { op, payload: body };
                total += spg_wire::parse_data_row_batch(&f)
                    .map(|r| r.len() as u64)
                    .unwrap_or(0);
            }
            Op::CommandComplete => break,
            _ => continue,
        }
    }
    total
}

fn wait_for_cold_segments(s: &mut TcpStream, want: usize) {
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        let total = count_stat_segment_rows(s) as usize;
        if total >= want {
            return;
        }
        if Instant::now() > deadline {
            panic!("server never produced {want} cold segments; got {total}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn sequential_scan_triggers_prefetch() {
    let dir = unique_tmpdir("hits");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: populate + freeze + CHECKPOINT so the manifest
    // lists ≥ 2 cold segments.
    let expected_hits: u64;
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_http()
            .env("SPG_HOT_TIER_BYTES", "32")
            .env("SPG_FREEZER_TICK_MS", "50")
            .env("SPG_FREEZER_BATCH_ROWS", "6")
            .spawn();
        {
            let mut s = common::connect_to(&addrs.native);
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            exec_ok(
                &mut s,
                "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
            );
            exec_ok(&mut s, "CREATE INDEX by_id ON t (id)");
            for i in 0..20i64 {
                exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, 'row-{i}')"));
            }
            wait_for_cold_segments(&mut s, 2);
            exec_ok(&mut s, "CHECKPOINT");
            // The boot prefetch pool walks the MANIFEST's live
            // segments — counting `seg_*.spg` files over-counts
            // whenever auto-compaction retired a segment whose disk
            // file is still present (tombstoned slots keep their
            // files; see EmbeddedMetrics::cold_segments). Take the
            // expected count from the live catalog instead — this
            // was the suite's recurring "hits 3 ≠ 4" flake.
            expected_hits = count_stat_segment_rows(&mut s);
        }
        let _ = raw.kill();
        let _ = raw.wait();
    }

    // Phase 2: restart from db_path. Boot path runs the prefetch
    // pool over the manifest-listed segments → metric increments
    // by the segment count.
    assert!(
        expected_hits >= 2,
        "phase-1 didn't leave ≥ 2 live cold segments"
    );

    let (mut raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .arg("-")
        .arg_path(&wal)
        .with_http()
        .env("SPG_FREEZER_DISABLE", "1")
        .env("SPG_PREFETCH_WORKERS", "4")
        .spawn();
    let _guard = common::ChildGuard(raw);
    let http_addr = addrs.http.as_ref().expect("http listener");
    // The boot prefetch pool is asynchronous — under a loaded host
    // the last worker may still be counting when /metrics is first
    // scraped. Poll to the expected value instead of asserting a
    // single early sample (v7.22; this was the suite's top flake).
    // v7.29 — 60 s: liveness guard, not a timing assertion. A fully
    // parallel suite starved the last worker past 15 s once (hits
    // stuck at 3/4); single-run completes in well under a second.
    let deadline = Instant::now() + Duration::from_secs(60);
    let hits = loop {
        let body = http_get_body(http_addr, "/metrics");
        let h = metric_value(&body, "spg_cold_prefetch_hits_total")
            .expect("spg_cold_prefetch_hits_total metric present");
        if h >= expected_hits || Instant::now() > deadline {
            break h;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if hits != expected_hits {
        // Forensics for the next occurrence: the full metrics page
        // says whether the worker died or is merely late.
        let body = http_get_body(http_addr, "/metrics");
        panic!("prefetch hits {hits} != on-disk segment count {expected_hits}; /metrics:\n{body}");
    }
}
