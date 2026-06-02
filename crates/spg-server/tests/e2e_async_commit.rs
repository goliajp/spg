#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

//! v5.4.2 — async-commit write path validation.
//!
//! v5.4.2 changes the WAL write path so that, when
//! `SPG_SYNCHRONOUS_COMMIT=off`, the per-write `sync_data` call
//! is skipped — the flusher thread (v5.4.1) handles durability
//! via periodic `durability_checkpoint` markers. The client's
//! commit acknowledgement returns immediately after the in-
//! memory engine apply.
//!
//! These tests pin two invariants:
//!   1. In-memory visibility is preserved (an INSERT becomes
//!      visible to the same connection's next SELECT, just like
//!      sync mode). If async mode silently skipped the in-memory
//!      apply, this would fail.
//!   2. The default sync mode is byte-for-byte unaffected by the
//!      env knob (writes still apply, reads still see them).
//!
//! Durability invariants (no data loss across clean shutdown,
//! bounded loss across kill -9 inside the async window) are the
//! province of v5.4.3's chaos test; throughput numbers belong to
//! v5.4.4's `slo_wal_insert_async_commit_above_200K` ship gate.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

mod common;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_command_complete,
    parse_data_row, parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-async-commit-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

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

fn expect_cc(stream: &mut TcpStream) {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn run_select(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => rows.extend(parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return rows,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

fn exercise_writes_under(env: &[(&str, String)], tag: &str) -> usize {
    let dir = unique_tmpdir(tag);
    let db = dir.join("data.spgdb");
    let wal = dir.join("data.wal");
    let (raw, addrs) = spawn_server(&db, &wal, env);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut s, "CREATE TABLE rows (id BIGINT, name TEXT)");
    expect_cc(&mut s);
    for i in 0..5_i64 {
        send_query(&mut s, &format!("INSERT INTO rows VALUES ({i}, 'row-{i}')"));
        expect_cc(&mut s);
    }
    let rows = run_select(&mut s, "SELECT id FROM rows");
    rows.len()
}

#[test]
fn sync_commit_default_writes_apply_and_are_visible() {
    // Default sync mode: nothing changes about the v4.42 invariant
    // that an INSERT is visible to the same connection's next
    // SELECT. This is the "no regression" gate for the v5.4.2
    // patch — if removing `sync_data` accidentally broke the
    // sync path, this test would fail.
    let count = exercise_writes_under(&[], "sync-default");
    assert_eq!(count, 5, "default sync mode must see all 5 inserts");
}

#[test]
fn async_commit_off_inserts_visible_immediately() {
    // Async-commit mode: client CC returns before fsync, but the
    // in-memory engine apply still happens before the ack — so a
    // follow-up SELECT on the same connection must see every row.
    // Flusher cadence kept short (500 µs) so any pending writes
    // hit durability quickly, but the test does not depend on
    // durability — just on in-memory visibility.
    let env = [
        ("SPG_SYNCHRONOUS_COMMIT".to_owned(), "off".to_owned()),
        ("SPG_FLUSHER_INTERVAL_US".to_owned(), "500".to_owned()),
    ];
    let env_refs: Vec<(&str, String)> = env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let count = exercise_writes_under(&env_refs, "async-off");
    assert_eq!(
        count, 5,
        "async-commit mode must keep in-memory visibility intact"
    );
}

#[test]
fn explicit_sync_commit_on_behaves_like_default() {
    // `SPG_SYNCHRONOUS_COMMIT=on` (explicit) must behave exactly
    // like the unset default — both keep the v4.42 sync semantics.
    let env_refs: Vec<(&str, String)> = vec![("SPG_SYNCHRONOUS_COMMIT", "on".to_owned())];
    let count = exercise_writes_under(&env_refs, "sync-explicit");
    assert_eq!(count, 5, "explicit sync mode must see all 5 inserts");
}
