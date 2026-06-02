#![allow(unused_mut, unused_variables)]
//! Transaction end-to-end:
//! - BEGIN / COMMIT applies + persists.
//! - BEGIN / ROLLBACK doesn't persist and the db file is untouched.
//! - Audit log gets exactly one entry per COMMIT (no entries for BEGIN /
//!   ROLLBACK / intra-TX writes).

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_audit::AuditLog;
use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

mod common;

fn local_spawn(
    db: &std::path::Path,
    audit: Option<&std::path::PathBuf>,
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new().arg_path(db);
    if let Some(a) = audit {
        b = b.arg_path(a);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(3);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-tx-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn expect_cc(stream: &mut TcpStream) {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

#[test]
fn begin_insert_commit_persists() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    {
        let (raw, addrs) = local_spawn(&db, None);
        let mut child = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
        expect_cc(&mut s);
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (2)");
        expect_cc(&mut s);
        send_query(&mut s, "COMMIT");
        expect_cc(&mut s);
    }

    // Restart and verify both rows survived.
    let (raw, addrs) = local_spawn(&db, None);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut s, "SELECT * FROM t");
    assert_eq!(read_frame(&mut s).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(&mut s);
        match f.op {
            Op::DataRow => count += 1,
            Op::DataRowBatch => count += spg_wire::parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(count, 2);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn begin_insert_rollback_leaves_db_file_unchanged() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db, None);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
    expect_cc(&mut s);
    // After CREATE TABLE (which is outside-TX writeful), db file exists with
    // a snapshot. Record its bytes.
    let bytes_before_tx = fs::read(&db).unwrap();

    send_query(&mut s, "BEGIN");
    expect_cc(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1)");
    expect_cc(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (2)");
    expect_cc(&mut s);
    send_query(&mut s, "ROLLBACK");
    expect_cc(&mut s);

    let bytes_after_tx = fs::read(&db).unwrap();
    assert_eq!(
        bytes_before_tx, bytes_after_tx,
        "the db file must not have been rewritten during the rolled-back TX"
    );

    // And the in-server state is empty too.
    send_query(&mut s, "SELECT * FROM t");
    assert_eq!(read_frame(&mut s).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(&mut s);
        match f.op {
            Op::DataRow => count += 1,
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(count, 0);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn audit_records_commit_only_not_intra_tx_or_rollback() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");

    {
        let (raw, addrs) = local_spawn(&db, Some(&audit));
        let mut child = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        // Outside TX → 1 audit entry per write.
        send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
        expect_cc(&mut s);

        // A whole TX → 1 audit entry (the COMMIT).
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (2)");
        expect_cc(&mut s);
        send_query(&mut s, "COMMIT");
        expect_cc(&mut s);

        // Rolled-back TX → 0 entries.
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (3)");
        expect_cc(&mut s);
        send_query(&mut s, "ROLLBACK");
        expect_cc(&mut s);
    }

    let bytes = fs::read(&audit).expect("audit file");
    let log = AuditLog::deserialize(&bytes).expect("audit verify");
    let sqls: Vec<&str> = log.entries().iter().map(|e| e.sql.as_str()).collect();
    assert_eq!(sqls, ["CREATE TABLE t (v INT NOT NULL)", "COMMIT"]);

    fs::remove_dir_all(&dir).ok();
}
