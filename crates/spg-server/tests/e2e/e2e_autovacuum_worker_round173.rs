//! read01 round 173 — server autovacuum runs in a background worker,
//! never inside a client statement (PG's shape).
//!
//! Pins, each on its own server:
//!   1. naptime = huge → the DML statement that crosses the dead-row
//!      threshold does NOT vacuum inline anymore (n_dead_tup stays
//!      up). Pre-r173 the crossing statement carried the vacuum.
//!   2. naptime = short → the worker reclaims WITHOUT any further
//!      DML on the table (proof it is background, not inline).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use std::process::Child;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_command_complete,
    parse_data_row, parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-autovac-worker-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(dir: &std::path::Path, naptime_ms: &str) -> (Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .env("SPG_AUTOVACUUM_NAPTIME_MS", naptime_ms.to_string())
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

fn as_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::BigInt(n) => *n,
        WireValue::Int(n) => i64::from(*n),
        other => panic!("expected int, got {other:?}"),
    }
}

fn dead_tup(stream: &mut TcpStream, table: &str) -> i64 {
    let rows = run_select(
        stream,
        &format!("SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = '{table}'"),
    );
    as_i64(&rows[0][0])
}

/// 1500 inserts + delete 1200 → dead=1200 >= 1000, live=300,
/// dead*4 >= live: over the autovacuum threshold.
fn create_bloat(stream: &mut TcpStream) {
    send_query(stream, "CREATE TABLE av (id BIGINT)");
    expect_cc(stream);
    send_query(
        stream,
        "INSERT INTO av SELECT g FROM generate_series(1, 1500) g",
    );
    expect_cc(stream);
    send_query(stream, "DELETE FROM av WHERE id <= 1200");
    expect_cc(stream);
}

#[test]
fn crossing_statement_no_longer_vacuums_inline() {
    let dir = unique_tmpdir("inline-off");
    // Naptime far beyond the test's lifetime — the worker will not
    // tick, so any reclaim we observe would have to be inline.
    let (raw, addrs) = spawn_server(&dir, "3600000");
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    create_bloat(&mut s);
    let d = dead_tup(&mut s, "av");
    assert!(
        d >= 1200,
        "server statement path must not vacuum inline (dead={d})"
    );
    assert_eq!(
        as_i64(&run_select(&mut s, "SELECT count(*) FROM av")[0][0]),
        300
    );
}

#[test]
fn worker_reclaims_without_further_dml() {
    let dir = unique_tmpdir("worker");
    let (raw, addrs) = spawn_server(&dir, "100");
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    create_bloat(&mut s);
    // No further DML on `av` — only polling reads. The background
    // worker must reclaim on its own cadence.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if dead_tup(&mut s, "av") == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not vacuum within 10s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        as_i64(&run_select(&mut s, "SELECT count(*) FROM av")[0][0]),
        300
    );
}
