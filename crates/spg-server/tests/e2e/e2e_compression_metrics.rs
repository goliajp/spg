//! v6.6.3 — `/metrics` exposes compression ratio counters after a
//! workload that exercises both WAL compression (v6.6.1) and
//! cold-tier segment compression (v6.6.2).
//!
//! Series asserted:
//!   spg_wal_bytes_uncompressed_total       > 0 after INSERTs
//!   spg_wal_bytes_compressed_total         > 0 and < uncompressed
//!   spg_segment_bytes_uncompressed_total   ≥ 0 (≥ 0 because the
//!     freezer may not have fired in the test window)

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{Op, build_query, encode, parse_error_response};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-compress-metrics-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn graceful_stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: libc::kill FFI, pid is a live PID from child.id().
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = child.wait();
}

fn send_query_via_native(s: &mut TcpStream, sql: &str) {
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

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query_via_native(s, sql);
    drain_until_cc(s, sql);
}

#[test]
fn wal_ratio_metrics_update_after_workload() {
    let dir = unique_tmpdir("wal");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    let (mut raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_http()
        .env("SPG_WAL", wal.to_string_lossy().into_owned())
        .env("SPG_WAL_COMPRESSION", "lzss")
        .spawn();

    {
        let mut s = TcpStream::connect(&addrs.native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_native(
            &mut s,
            "CREATE TABLE t (id INT NOT NULL, payload TEXT NOT NULL)",
        );
        for i in 0..50 {
            let sql = format!(
                "INSERT INTO t VALUES ({i}, '{}')",
                "compressible payload alpha beta gamma ".repeat(8)
            );
            exec_native(&mut s, &sql);
        }
    }

    let http = addrs.http.expect("http listener");
    let metrics = http_get(&http, "/metrics");
    eprintln!("metrics excerpt:\n{}", metrics);

    let raw_in = parse_counter(&metrics, "spg_wal_bytes_uncompressed_total");
    let comp_out = parse_counter(&metrics, "spg_wal_bytes_compressed_total");
    assert!(raw_in > 0, "expected uncompressed_in > 0, got {raw_in}");
    assert!(comp_out > 0, "expected compressed_out > 0, got {comp_out}");
    assert!(
        comp_out < raw_in,
        "expected compressed_out < uncompressed_in; got {comp_out} vs {raw_in}"
    );

    graceful_stop(&mut raw);
}

#[test]
fn segment_ratio_series_present_in_metrics() {
    // Don't try to exercise the freezer (it has its own thresholds
    // and timing); just verify the counters render at zero on a
    // freshly-started server.
    let dir = unique_tmpdir("seg");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    let (mut raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_http()
        .env("SPG_WAL", wal.to_string_lossy().into_owned())
        .spawn();

    let http = addrs.http.expect("http listener");
    let metrics = http_get(&http, "/metrics");
    assert!(metrics.contains("spg_segment_bytes_uncompressed_total"));
    assert!(metrics.contains("spg_segment_bytes_compressed_total"));
    graceful_stop(&mut raw);
}

fn http_get(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let txt = String::from_utf8_lossy(&buf).to_string();
    // Strip headers; keep body.
    if let Some(idx) = txt.find("\r\n\r\n") {
        txt[idx + 4..].to_string()
    } else {
        txt
    }
}

fn parse_counter(metrics: &str, key: &str) -> u64 {
    for line in metrics.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(key)
            && let Some(val_str) = rest.split_whitespace().next()
            && let Ok(v) = val_str.parse::<u64>()
        {
            return v;
        }
    }
    panic!("counter {key:?} not found in /metrics output");
}
