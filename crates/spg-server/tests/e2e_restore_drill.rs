#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]

//! v4.30 — RESTORE_DRILL.md as executable test.
//!
//! Runs the exact command sequence from RESTORE_DRILL.md against
//! a freshly-built backup pair (one full + one incremental). The
//! test fails if either:
//!
//!   - the recovered server can't be started, or
//!   - the recovered row count doesn't match what was CC'd to the
//!     primary before backups were taken.
//!
//! Coverage:
//!   * Step 1 (apply full + incremental bundles)
//!   * Step 3 (start the recovered server)
//!   * Step 4 (verify with a row count)
//!   * Step 2 PITR variant (SPG_REPLAY_UPTO=0) via a second probe
//!
//! Step 0 (inspect) is a read-only sanity check — covered by the
//! v4.25 e2e_backup test elsewhere. Step 5 (follower
//! re-bootstrap) is covered by e2e_replication.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use std::thread;

mod common;

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-drill-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    let f = read_frame(s);
    if f.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

fn exec_with_count(s: &mut TcpStream, sql: &str) -> u64 {
    send(s, &build_query(sql));
    let f = read_frame(s);
    if f.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(f.op, Op::CommandComplete);
    spg_wire::parse_command_complete(&f).unwrap()
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut count: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => count = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                count = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return count,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Decode a bundle (RESTORE_DRILL.md step 0 + helper for step 1).
/// Returns (kind, since, snap_len, wal_pos, wal_len, wal_slice_start).
fn parse_bundle_header(bytes: &[u8]) -> (u8, u64, usize, u64, usize, usize) {
    // v4.37: writers emit \x02 (with trailing CRC32). Restorers
    // accept either.
    assert!(
        &bytes[..8] == b"SPGBKUP\x01" || &bytes[..8] == b"SPGBKUP\x02",
        "not an SPG bundle (magic = {:?})",
        &bytes[..8]
    );
    let kind = bytes[8];
    let since = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
    let snap_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap()) as usize;
    let snap_end = 33 + snap_len;
    let wal_pos = u64::from_le_bytes(bytes[snap_end..snap_end + 8].try_into().unwrap());
    let wal_len =
        u64::from_le_bytes(bytes[snap_end + 8..snap_end + 16].try_into().unwrap()) as usize;
    let wal_start = snap_end + 16;
    (kind, since, snap_len, wal_pos, wal_len, wal_start)
}

/// RESTORE_DRILL.md step 1: apply a bundle.
fn apply_bundle(rec_db: &Path, rec_wal: &Path, bundle_path: &Path) {
    let bytes = std::fs::read(bundle_path).unwrap();
    let (_kind, _since, snap_len, _wal_pos, wal_len, wal_start) = parse_bundle_header(&bytes);
    let wal_slice = &bytes[wal_start..wal_start + wal_len];
    if snap_len > 0 {
        std::fs::write(rec_db, &bytes[33..33 + snap_len]).unwrap();
        std::fs::write(rec_wal, wal_slice).unwrap();
    } else {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(rec_wal)
            .unwrap();
        f.write_all(wal_slice).unwrap();
        f.sync_data().unwrap();
    }
}

#[test]
fn restore_drill_full_plus_incremental_recovers_row_count() {
    // ---- Source side: produce a full + incremental bundle ----
    let src_dir = tmpdir("src");
    let src_db = src_dir.join("a.db");
    let src_wal = src_dir.join("a.wal");
    let src_full = src_dir.join("full.bkp");
    let src_incr = src_dir.join("incr.bkp");

    let (raw, src_addrs) = local_spawn(&src_db, &src_wal, &[]);
    let mut src_child = common::ChildGuard(raw);
    let mut s = common::connect_to(&src_addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    for i in 0..50 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, {})", i * 3));
    }
    let pivot = exec_with_count(&mut s, &format!("BACKUP TO '{}'", src_full.display()));
    for i in 50..70 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, {})", i * 3));
    }
    let _incr_pivot = exec_with_count(
        &mut s,
        &format!(
            "BACKUP TO '{}' INCREMENTAL SINCE {pivot}",
            src_incr.display()
        ),
    );

    drop(s);
    let _ = src_child.0.kill();
    let _ = src_child.0.wait();
    thread::sleep(Duration::from_millis(200));

    // ---- Recovery side: follow RESTORE_DRILL.md step 1 + step 3 ----
    let rec_dir = tmpdir("rec");
    let rec_db = rec_dir.join("rec.db");
    let rec_wal = rec_dir.join("rec.wal");

    // Step 1.2 (apply full).
    apply_bundle(&rec_db, &rec_wal, &src_full);
    // Step 1.3 (apply each incremental in order).
    apply_bundle(&rec_db, &rec_wal, &src_incr);

    // Step 3 (start recovered server).
    let (raw, rec_addrs) = local_spawn(&rec_db, &rec_wal, &[]);
    let mut rec_child = common::ChildGuard(raw);
    let mut rs = common::connect_to(&rec_addrs.native);
    rs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Step 4 (verify).
    let after = select_int(&mut rs, "SELECT count(*) FROM t");
    assert_eq!(
        after, 70,
        "expected 70 rows (50 baseline + 20 incremental), got {after}"
    );
    let _ = rec_child.0.kill();
    let _ = rec_child.0.wait();
    thread::sleep(Duration::from_millis(200));

    // ---- Step 2 variant: PITR via SPG_REPLAY_UPTO=0 ----
    // Snapshot-only recovery from the same files. The full
    // bundle's snapshot already encodes 50 rows; SPG_REPLAY_UPTO=0
    // skips the appended incremental WAL slice entirely.
    let (raw, rec_addr2_addrs) =
        local_spawn(&rec_db, &rec_wal, &[("SPG_REPLAY_UPTO", "0".to_string())]);
    let _rec_child2 = common::ChildGuard(raw);
    let mut rs2 = common::connect_to(&rec_addr2_addrs.native);
    rs2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after_pitr = select_int(&mut rs2, "SELECT count(*) FROM t");
    assert_eq!(
        after_pitr, 50,
        "PITR with SPG_REPLAY_UPTO=0 must roll back to the snapshot (50 rows), got {after_pitr}"
    );
}
