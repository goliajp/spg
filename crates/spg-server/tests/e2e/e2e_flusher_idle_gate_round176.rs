//! read01 round 176 — the flusher only fsyncs when the WAL grew.
//!
//! Root cause pinned this round: the v5.4.1 flusher default of one
//! durability marker + fsync per 200 µs meant an ~100% fsync duty
//! cycle on the WAL file (one APFS fsync is ~5-8 ms), so every
//! concurrent client `append_wal` write_all stalled ~1-3 ms behind
//! the in-flight fsync — the SPGS wire-panel per-statement tax
//! (r175: sync=off singles 4 ms/stmt). Two changes:
//!   * default interval 200 µs → 200 ms (PG wal_writer_delay).
//!   * idle gate: no marker/fsync when the WAL hasn't grown past
//!     the last durable marker.
//! This test pins the idle gate: an async-mode server with no write
//! traffic must not keep emitting markers; traffic must resume them.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use spg_wire::{FRAME_HEADER_LEN, Frame, Op, build_query, encode};

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

/// Hand-rolled GET (same shape as e2e_observability's helper).
fn http_get(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect http");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    buf
}

fn flusher_iterations(http: &str) -> u64 {
    let body = http_get(http, "/metrics");
    body.lines()
        .find(|l| l.starts_with("spg_flusher_iterations_total"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .expect("flusher metric present")
}

#[test]
fn idle_async_server_stops_emitting_markers() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("spg-flusher-idle-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .env("SPG_SYNCHRONOUS_COMMIT", "off")
        .env("SPG_FLUSHER_INTERVAL_US", "20000") // 20 ms — fast test cadence
        .with_http()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let http = addrs.http.clone().expect("http listener");
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // One write so the first marker (covering the CREATE) lands, then
    // wait for the flusher to go idle: iterations must plateau.
    send_query(&mut s, "CREATE TABLE t (id BIGINT)");
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    send_query(&mut s, "INSERT INTO t VALUES (1)");
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    std::thread::sleep(Duration::from_millis(200));
    let settled = flusher_iterations(&http);
    std::thread::sleep(Duration::from_millis(400)); // ≥ 20 idle ticks
    let after_idle = flusher_iterations(&http);
    assert!(
        after_idle <= settled + 1,
        "idle server must not keep emitting markers: {settled} -> {after_idle}"
    );

    // New traffic resumes marker coverage.
    send_query(&mut s, "INSERT INTO t VALUES (2)");
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if flusher_iterations(&http) > after_idle {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "flusher must resume after new WAL bytes"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
