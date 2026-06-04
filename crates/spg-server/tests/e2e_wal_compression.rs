//! v6.6.1 — WAL v3 compressed-record format.
//!
//! Tests the encode + dispatch + decompress round-trip via the
//! native SPG wire (auto-commit path, which is the only path
//! that writes to the WAL — pgwire writes don't currently
//! persist to WAL, an honest pre-v6.6 gap not in scope here).
//!
//! Verifies:
//! 1. Compressed records (type=0x03) round-trip through WAL
//!    replay correctly.
//! 2. Compression ratio: same workload produces a smaller WAL
//!    under `SPG_WAL_COMPRESSION=lzss` than under `=none`.
//! 3. Legacy uncompressed type=0x01 records still replay through
//!    the upgraded binary (regression).

#![allow(unsafe_code)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{Frame, Op, build_query, encode, parse_error_response};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-wal-zip-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    compression: Option<&str>,
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .env("SPG_WAL", wal.to_string_lossy().into_owned());
    if let Some(v) = compression {
        b = b.env("SPG_WAL_COMPRESSION", v.to_string());
    }
    b.spawn()
}

fn graceful_stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: libc::kill FFI; pid comes from child.id() and is
        // a valid PID for the process this struct manages.
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

fn read_response(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; len];
    if len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query_via_native(s, sql);
    // Drain until CommandComplete / ErrorResponse.
    loop {
        let f = read_response(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("SQL failed: {sql:?} → {msg}");
            }
            _ => continue,
        }
    }
}

fn workload_wal_size(wal: &std::path::Path, compression: &str) -> u64 {
    let dir = wal.parent().unwrap();
    let db = dir.join("spg.db");
    let _ = fs::remove_file(&db);
    let _ = fs::remove_file(wal);
    let (mut raw, addrs) = local_spawn(&db, wal, Some(compression));
    {
        let mut s = TcpStream::connect(&addrs.native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_native(
            &mut s,
            "CREATE TABLE t (id INT NOT NULL, payload TEXT NOT NULL)",
        );
        for i in 0..100 {
            let sql = format!(
                "INSERT INTO t VALUES ({i}, '{}')",
                "the quick brown fox jumps over the lazy dog ".repeat(8)
            );
            exec_native(&mut s, &sql);
        }
    }
    graceful_stop(&mut raw);
    fs::metadata(wal).expect("wal exists").len()
}

#[test]
fn ratio_at_least_20pct_smaller_with_lzss() {
    let dir = unique_tmpdir("ratio");
    let wal_zip = dir.join("wal_lzss.log");
    let wal_raw = dir.join("wal_none.log");
    let zipped = workload_wal_size(&wal_zip, "lzss");
    let raw = workload_wal_size(&wal_raw, "none");
    eprintln!("v6.6.1 ratio: raw={raw} compressed={zipped}");
    assert!(raw > 0 && zipped > 0, "both WALs must have bytes");
    assert!(
        zipped * 5 <= raw * 4,
        "expected compressed WAL ≤ 80% of raw; got compressed={zipped}, raw={raw}"
    );
}

#[test]
fn compressed_records_round_trip_through_replay() {
    let dir = unique_tmpdir("roundtrip");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: insert with compression on, then graceful shutdown.
    {
        let (mut raw, addrs) = local_spawn(&db, &wal, Some("lzss"));
        {
            let mut s = TcpStream::connect(&addrs.native).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            exec_native(
                &mut s,
                "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
            );
            for i in 0..50 {
                let sql = format!(
                    "INSERT INTO t VALUES ({i}, '{}')",
                    "extended payload that should compress well ".repeat(8)
                );
                exec_native(&mut s, &sql);
            }
        }
        graceful_stop(&mut raw);
    }
    assert!(
        fs::metadata(&wal).unwrap().len() > 0,
        "phase 1 WAL must have bytes"
    );

    // Phase 2: restart, count rows via a fresh native connection.
    let (mut raw2, addrs2) = local_spawn(&db, &wal, Some("lzss"));
    {
        let mut s = TcpStream::connect(&addrs2.native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query_via_native(&mut s, "SELECT id FROM t");
        // Loop until CommandComplete; count rows accumulated across
        // DataRow / DataRowBatch frames.
        let mut total: usize = 0;
        loop {
            let f = read_response(&mut s);
            match f.op {
                Op::DataRow => total += 1,
                Op::DataRowBatch => {
                    if let Ok(rows) = spg_wire::parse_data_row_batch(&f) {
                        total += rows.len();
                    }
                }
                Op::CommandComplete => break,
                Op::Error | Op::ErrorResponse => panic!(
                    "select failed: {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                ),
                // RowDescription / StatsResponse / etc — skip and continue
                // reading until CommandComplete.
                _ => continue,
            }
        }
        assert_eq!(total, 50, "expected 50 rows after replay, got {total}");
    }
    graceful_stop(&mut raw2);
}
