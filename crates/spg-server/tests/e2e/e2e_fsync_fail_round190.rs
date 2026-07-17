//! read01 round 190 (D13) — a failed fsync must not leave its bytes
//! in the WAL for the NEXT successful fsync to make durable.
//!
//! Pre-r190 sequence: group append (write_all) → fsync FAILS →
//! in-memory rollback + client error — but the appended bytes stayed
//! in the file. The next successful commit's fsync flushed them too,
//! and boot replay resurrected the rolled-back statement: the client
//! was told the write FAILED, yet after a restart the row exists
//! (silent-wrong). r190 truncates the group's bytes back off under
//! the still-held WAL mutex before surfacing the error.
//!
//! Chaos knob: SPG_FAIL_FSYNC_AT=K — the K-th client-path sync_data
//! fails once with injected EIO.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, build_query, encode, parse_data_row, parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-fsync-fail-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(dir: &std::path::Path, env: &[(&str, &str)]) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"));
    for (k, v) in env {
        b = b.env(*k, (*v).to_string());
    }
    b.spawn()
}

fn send(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn frame(stream: &mut TcpStream) -> Frame {
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

/// Run one statement; true = CommandComplete, false = error frame.
fn exec_ok(stream: &mut TcpStream, sql: &str) -> bool {
    send(stream, sql);
    loop {
        let f = frame(stream);
        match f.op {
            Op::CommandComplete => return true,
            Op::RowDescription | Op::DataRow | Op::DataRowBatch => {}
            _ => return false,
        }
    }
}

fn count(stream: &mut TcpStream, sql: &str) -> usize {
    send(stream, sql);
    assert_eq!(frame(stream).op, Op::RowDescription);
    let mut n = 0;
    loop {
        let f = frame(stream);
        match f.op {
            Op::DataRow => n += parse_data_row(&f).map(|_| 1).unwrap_or(1),
            Op::DataRowBatch => n += parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => return n,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn failed_fsync_write_stays_dead_after_restart() {
    let dir = unique_tmpdir();
    {
        // fsync #1 = CREATE TABLE, #2 = INSERT a, #3 = INSERT b
        // (injected failure), #4 = INSERT c.
        let (raw, addrs) = spawn_server(&dir, &[("SPG_FAIL_FSYNC_AT", "3")]);
        let mut guard = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        assert!(exec_ok(&mut s, "CREATE TABLE t (id BIGINT)"));
        assert!(exec_ok(&mut s, "INSERT INTO t VALUES (1)"));
        assert!(
            !exec_ok(&mut s, "INSERT INTO t VALUES (2)"),
            "the injected fsync failure must surface as a client error"
        );
        // The server closes the connection after a WAL failure (its
        // v4.41.1 contract — milder than PG's fsync PANIC); reconnect.
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        assert!(exec_ok(&mut s, "INSERT INTO t VALUES (3)"));
        assert_eq!(
            count(&mut s, "SELECT id FROM t"),
            2,
            "in-memory state: rolled-back row absent"
        );
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir, &[]);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        count(&mut s, "SELECT id FROM t"),
        2,
        "replay must NOT resurrect the failed-fsync row (client was told it failed)"
    );
    assert_eq!(count(&mut s, "SELECT id FROM t WHERE id = 2"), 0);
}
