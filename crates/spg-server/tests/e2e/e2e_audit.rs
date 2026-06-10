//! BLAKE3 hash-chain audit log end-to-end:
//! - Every successful DML appears in the on-disk audit file in order.
//! - A fresh daemon started against an unmodified audit file accepts it.
//! - A fresh daemon started against a tampered file refuses to come up.

use crate::common;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_audit::AuditLog;
use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

fn local_spawn(
    db: &std::path::Path,
    audit: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg_path(audit)
        .spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(3);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-audit-e2e-{pid}-{nanos}-{serial}"));
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
fn audit_chain_grows_with_each_writeful_query() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");
    let (raw, addrs) = local_spawn(&db, &audit);
    let mut child = common::ChildGuard(raw);
    let mut stream = common::connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut stream, "CREATE TABLE t (v INT)");
    expect_cc(&mut stream);
    send_query(&mut stream, "INSERT INTO t VALUES (1)");
    expect_cc(&mut stream);
    send_query(&mut stream, "INSERT INTO t VALUES (2)");
    expect_cc(&mut stream);
    // SELECT must NOT add an audit entry — it's read-only.
    send_query(&mut stream, "SELECT * FROM t");
    assert_eq!(read_frame(&mut stream).op, Op::RowDescription);
    while read_frame(&mut stream).op != Op::CommandComplete {}
    drop(child);

    let bytes = fs::read(&audit).expect("audit file exists");
    let log = AuditLog::deserialize(&bytes).expect("audit verify on read-back");
    assert_eq!(log.len(), 3);
    assert_eq!(log.entries()[0].sql, "CREATE TABLE t (v INT)");
    assert_eq!(log.entries()[1].sql, "INSERT INTO t VALUES (1)");
    assert_eq!(log.entries()[2].sql, "INSERT INTO t VALUES (2)");
    // Chain integrity.
    assert_eq!(log.entries()[0].prev_hash, [0u8; 32]);
    assert_eq!(log.entries()[1].prev_hash, log.entries()[0].hash);
    assert_eq!(log.entries()[2].prev_hash, log.entries()[1].hash);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fresh_daemon_accepts_intact_audit_log_on_restart() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");

    // Phase 1: produce.
    {
        let (raw, addrs) = local_spawn(&db, &audit);
        let mut child = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
    }

    // Phase 2: restart, sanity-ping.
    let (raw, addrs) = local_spawn(&db, &audit);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut out = Vec::new();
    encode(&Frame::ping(), &mut out).unwrap();
    s.write_all(&out).unwrap();
    assert_eq!(read_frame(&mut s).op, Op::Pong);
    drop(child);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn tampered_audit_log_makes_daemon_fail_startup() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");

    // Phase 1: produce a valid 2-entry log.
    {
        let (raw, addrs) = local_spawn(&db, &audit);
        let mut child = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
    }

    // Phase 2: tamper — flip the very last byte of the file (inside the
    // second entry's SQL bytes, after the hash field).
    let mut bytes = fs::read(&audit).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&audit, &bytes).unwrap();

    // Phase 3: spawn fresh daemon, expect it to exit non-zero within timeout.
    let mut child = common::ServerBuilder::new()
        .arg_path(&db)
        .arg_path(&audit)
        .spawn_expecting_startup_failure();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                !status.success(),
                "server should fail startup, exited {status:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server kept running with tampered audit log");
        }
        thread::sleep(Duration::from_millis(50));
    }

    fs::remove_dir_all(&dir).ok();
}
