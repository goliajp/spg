//! read01 round 177 — explicit transactions fsync once at COMMIT,
//! not per statement (PG semantics).
//!
//! Pre-r177 every statement inside BEGIN…COMMIT paid its own
//! `sync_data` on the WAL (tx_batch_100 = 100 fsyncs ≈ 740 ms — the
//! r175 wire panel's 24× worst cell). Now a statement that leaves the
//! engine inside an open transaction appends without fsync; the
//! COMMIT (which closes it) fsyncs once, covering the whole tx.
//!
//! Durability pins (the contract that must survive the optimization):
//!   1. kill -9 AFTER the COMMIT ack → every tx row survives restart.
//!   2. kill -9 MID-transaction (no COMMIT) → restart boots clean and
//!      the dangling tx is rolled back (rows absent) — same as PG
//!      losing an uncommitted tx.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use std::process::Child;

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
    let p = std::env::temp_dir().join(format!("spg-tx-fsync-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(dir: &std::path::Path) -> (Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .spawn()
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

#[test]
fn committed_tx_survives_kill_after_commit_ack() {
    let dir = unique_tmpdir("commit");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (id BIGINT)");
        expect_cc(&mut s);
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        for i in 0..20_i64 {
            send_query(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            expect_cc(&mut s);
        }
        send_query(&mut s, "COMMIT");
        expect_cc(&mut s);
        // COMMIT acked — its fsync must have covered the whole tx.
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        run_select(&mut s, "SELECT id FROM t").len(),
        20,
        "acked COMMIT must be durable across kill"
    );
}

#[test]
fn dangling_tx_rolls_back_on_restart() {
    let dir = unique_tmpdir("dangling");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (id BIGINT)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (100)");
        expect_cc(&mut s);
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        for i in 0..10_i64 {
            send_query(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            expect_cc(&mut s);
        }
        // No COMMIT — kill mid-transaction.
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let rows = run_select(&mut s, "SELECT id FROM t");
    assert_eq!(
        rows.len(),
        1,
        "dangling tx must roll back on replay; only the autocommit row survives"
    );
}
