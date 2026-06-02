#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]

//! v4.25 backup bundles + PITR — full backup, incremental backup,
//! and SPG_REPLAY_UPTO point-in-time recovery.
//!
//! Verifies operator-driven recovery from a backup file by:
//!   1. Run server, INSERT some rows.
//!   2. `BACKUP TO '<path>'` to capture a full bundle.
//!   3. INSERT more rows.
//!   4. `BACKUP TO '<path2>' INCREMENTAL SINCE <pos>` to ship deltas.
//!   5. Stop server, recover by feeding bundles into a fresh
//!      db_path + wal_path, restart, verify state matches.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

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

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-bkup-{nanos}"));
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

/// Send a SQL whose CC carries an affected-rows count and return it.
fn exec_with_count(s: &mut TcpStream, sql: &str) -> u64 {
    send(s, &build_query(sql));
    let f = read_frame(s);
    if f.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
    spg_wire::parse_command_complete(&f).unwrap()
}

fn select_count(s: &mut TcpStream, sql: &str) -> i64 {
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
            Op::DataRow => {
                let row = parse_data_row(&f).unwrap();
                count = wire_to_i64(&row[0]);
            }
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

/// Decode a bundle file (matching crates/spg-server/src/backup.rs
/// format) and append its WAL slice to `dest_wal`. The snapshot is
/// written to `dest_db` if `snap_len > 0`. Returns (kind, since, wal_pos).
fn apply_bundle_to(dest_db: &Path, dest_wal: &Path, bundle: &Path) -> (u8, u64, u64) {
    let bytes = std::fs::read(bundle).unwrap();
    // v4.37: writers emit \x02 (with trailing CRC32). Restorers
    // still accept \x01 — see backup.rs doc-comment.
    assert!(
        &bytes[..8] == b"SPGBKUP\x01" || &bytes[..8] == b"SPGBKUP\x02",
        "unknown bundle magic {:?}",
        &bytes[..8]
    );
    let kind = bytes[8];
    let since = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
    // bytes[17..25] = ts (skip)
    let snap_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap()) as usize;
    let snap_end = 33 + snap_len;
    let wal_pos = u64::from_le_bytes(bytes[snap_end..snap_end + 8].try_into().unwrap());
    let wal_len =
        u64::from_le_bytes(bytes[snap_end + 8..snap_end + 16].try_into().unwrap()) as usize;
    let wal_start = snap_end + 16;
    let wal_slice = &bytes[wal_start..wal_start + wal_len];

    if snap_len > 0 {
        std::fs::write(dest_db, &bytes[33..33 + snap_len]).unwrap();
        // Full backup → reset WAL to its captured slice.
        std::fs::write(dest_wal, wal_slice).unwrap();
    } else {
        // Incremental → append WAL slice.
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dest_wal)
            .unwrap();
        f.write_all(wal_slice).unwrap();
        f.sync_data().unwrap();
    }
    (kind, since, wal_pos)
}

#[test]
fn full_plus_incremental_round_trip_restores_state() {
    let dir = unique_tmpdir();
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");

    let (raw, addrs) = local_spawn(&db, &wal, &[]);
    let child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE k (id INT NOT NULL, v INT NOT NULL)");
    for i in 1..=3 {
        exec_ok(&mut s, &format!("INSERT INTO k VALUES ({i}, {})", i * 10));
    }

    let full_bundle = dir.join("full.bkp");
    let pos_after_full = exec_with_count(&mut s, &format!("BACKUP TO '{}'", full_bundle.display()));

    for i in 4..=5 {
        exec_ok(&mut s, &format!("INSERT INTO k VALUES ({i}, {})", i * 10));
    }

    let incr_bundle = dir.join("incr.bkp");
    let pos_after_incr = exec_with_count(
        &mut s,
        &format!(
            "BACKUP TO '{}' INCREMENTAL SINCE {}",
            incr_bundle.display(),
            pos_after_full
        ),
    );
    assert!(pos_after_incr > pos_after_full);

    // Tear down master.
    drop(s);
    drop(child);

    // Recover on a fresh node: apply full bundle (sets db + wal),
    // then incremental bundle (appends to wal).
    let rec_dir = unique_tmpdir();
    let rec_db = rec_dir.join("rec.db");
    let rec_wal = rec_dir.join("rec.wal");
    let (kind_full, _, _) = apply_bundle_to(&rec_db, &rec_wal, &full_bundle);
    assert_eq!(kind_full, 0, "expected full bundle marker");
    let (kind_incr, since_incr, _) = apply_bundle_to(&rec_db, &rec_wal, &incr_bundle);
    assert_eq!(kind_incr, 1, "expected incremental marker");
    assert_eq!(since_incr, pos_after_full);

    let (raw, rec_addrs) = local_spawn(&rec_db, &rec_wal, &[]);
    let _rec_child = common::ChildGuard(raw);
    let mut rs = common::connect_to(&rec_addrs.native);
    rs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    assert_eq!(select_count(&mut rs, "SELECT count(*) FROM k"), 5);
}

#[test]
fn pitr_via_replay_upto_truncates_history() {
    let dir = unique_tmpdir();
    let db = dir.join("p.db");
    let wal = dir.join("p.wal");

    let (raw, addrs) = local_spawn(&db, &wal, &[]);
    let child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE p (id INT NOT NULL)");
    exec_ok(&mut s, "INSERT INTO p VALUES (1)");
    exec_ok(&mut s, "INSERT INTO p VALUES (2)");

    let bundle = dir.join("snap.bkp");
    let pivot_pos = exec_with_count(&mut s, &format!("BACKUP TO '{}'", bundle.display()));

    // More writes after the pivot — these get captured in the WAL
    // (and on disk), but PITR will truncate them at startup.
    exec_ok(&mut s, "INSERT INTO p VALUES (3)");
    exec_ok(&mut s, "INSERT INTO p VALUES (4)");

    drop(s);
    drop(child);

    // Recover at the pivot: SPG_REPLAY_UPTO = pivot_pos truncates
    // WAL replay there, so only rows 1+2 should be present.
    let (raw, rec_addrs) = local_spawn(&db, &wal, &[("SPG_REPLAY_UPTO", pivot_pos.to_string())]);
    let _rec_child = common::ChildGuard(raw);
    let mut rs = common::connect_to(&rec_addrs.native);
    rs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    assert_eq!(select_count(&mut rs, "SELECT count(*) FROM p"), 2);
}
