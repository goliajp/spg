#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.1.2 — `CREATE PUBLICATION` / `DROP PUBLICATION` end-to-end.
//!
//! Native wire protocol round-trips through `spg-server`. The
//! engine-level invariants (duplicate-name error, drop-absent no-op,
//! v3 envelope persistence) are covered in `spg-engine`'s lib tests;
//! this file exercises the server boundary — WAL replay across a
//! process restart and PG-wire command tags via the native wire.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

mod common;

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new().arg_path(db).spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(3);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-pub-e2e-{pid}-{nanos}-{serial}"));
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

fn expect_cc(stream: &mut TcpStream) -> u64 {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap()
}

fn expect_err(stream: &mut TcpStream) -> String {
    let f = read_frame(stream);
    assert_eq!(f.op, Op::ErrorResponse, "expected ErrorResponse, got {:?}", f.op);
    spg_wire::parse_error_response(&f)
        .unwrap_or("<undecodable>")
        .to_string()
}

#[test]
fn create_publication_roundtrip_basic() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE PUBLICATION pub_a");
    // Native wire's CommandComplete carries the affected-row
    // count (u64). For DDL it's 1 — matches `CREATE TABLE` etc.
    let affected = expect_cc(&mut s);
    assert_eq!(affected, 1, "CREATE PUBLICATION affected=1");

    send_query(&mut s, "DROP PUBLICATION pub_a");
    let affected = expect_cc(&mut s);
    assert_eq!(affected, 1, "DROP existing publication affected=1");

    // DROP after first drop is a silent no-op → affected=0.
    send_query(&mut s, "DROP PUBLICATION pub_a");
    let affected = expect_cc(&mut s);
    assert_eq!(affected, 0, "DROP absent publication affected=0");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_publication_name_errors() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE PUBLICATION pub_a");
    expect_cc(&mut s);
    send_query(&mut s, "CREATE PUBLICATION pub_a");
    let err = expect_err(&mut s);
    assert!(
        err.contains("DuplicateName") || err.contains("pub_a"),
        "got: {err}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_nonexistent_publication_succeeds_silently() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "DROP PUBLICATION never_existed");
    expect_cc(&mut s); // must succeed

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn publications_persist_across_process_restart() {
    // Ship-gate from V6_1_DESIGN.md L3a Step 7. Master CREATE-s a
    // publication, server stops, server restarts; the publication
    // must still be present (verified by a duplicate CREATE failing).
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    {
        let (raw, addrs) = local_spawn(&db);
        let mut child = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE PUBLICATION pub_persist");
        expect_cc(&mut s);
        // Some non-trivial DML to flush the WAL too.
        send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
    }

    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Duplicate CREATE proves the publication survived the restart.
    send_query(&mut s, "CREATE PUBLICATION pub_persist");
    let err = expect_err(&mut s);
    assert!(
        err.contains("DuplicateName") || err.contains("pub_persist"),
        "publication must still exist after restart, got: {err}"
    );

    // DROP after restart still works.
    send_query(&mut s, "DROP PUBLICATION pub_persist");
    expect_cc(&mut s);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn for_all_tables_explicit_works() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE PUBLICATION pub_a FOR ALL TABLES");
    expect_cc(&mut s);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn for_table_list_errors_with_version_hint() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE PUBLICATION pub_a FOR TABLE t1, t2");
    let err = expect_err(&mut s);
    assert!(err.contains("v6.1.3"), "got: {err}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn for_all_tables_except_errors_with_version_hint() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE PUBLICATION pub_a FOR ALL TABLES EXCEPT t3");
    let err = expect_err(&mut s);
    assert!(err.contains("v6.1.3"), "got: {err}");

    fs::remove_dir_all(&dir).ok();
}
