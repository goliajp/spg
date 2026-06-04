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

#![allow(clippy::uninlined_format_args)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_error_response};

mod common;

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

fn wait_for_cold_segments(s: &mut TcpStream, want: usize) {
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        send_query(s, "SELECT * FROM spg_stat_segment");
        let mut total = 0usize;
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
                        .map(|r| r.len())
                        .unwrap_or(0);
                }
                Op::CommandComplete => break,
                _ => continue,
            }
        }
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
        }
        let _ = raw.kill();
        let _ = raw.wait();
    }

    // Phase 2: restart from db_path. Boot path runs the prefetch
    // pool over the manifest-listed segments → metric increments
    // by the segment count.
    let segments_dir = {
        let parent = db.parent().unwrap_or_else(|| std::path::Path::new("."));
        let stem = db.file_stem().unwrap().to_string_lossy().into_owned();
        parent.join(format!("{stem}.spg")).join("segments")
    };
    let expected_hits = std::fs::read_dir(&segments_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("seg_") && n.ends_with(".spg"))
                })
                .count()
        })
        .unwrap_or(0) as u64;
    assert!(
        expected_hits >= 2,
        "phase-1 didn't leave ≥ 2 cold segments on disk"
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
    let body = http_get_body(http_addr, "/metrics");
    let hits = metric_value(&body, "spg_cold_prefetch_hits_total")
        .expect("spg_cold_prefetch_hits_total metric present");
    assert_eq!(
        hits, expected_hits,
        "prefetch hits {hits} ≠ on-disk segment count {expected_hits}"
    );
}
