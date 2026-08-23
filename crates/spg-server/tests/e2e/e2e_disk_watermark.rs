//! v4.33 disk water-mark — `SPG_WAL_MIN_FREE_BYTES`. When the WAL
//! volume's free space is below the threshold, writes are refused
//! with a clear error; reads keep serving; the server stays alive.
//!
//! Test strategy: set the threshold to a value larger than any
//! real filesystem free space (u64::MAX / 2), so the water-mark is
//! guaranteed to trigger on every write attempt. Assert reads still
//! succeed and the server stays responsive.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use std::process::Child;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-watermark-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_db_wal(
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

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Error(String),
}

fn run_query(s: &mut TcpStream, sql: &str) -> Outcome {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return Outcome::Ok,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f)
                    .map_or_else(|_| "<undecodable>".into(), str::to_owned);
                return Outcome::Error(msg);
            }
            _ => {}
        }
    }
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

/// SPG_WAL_MIN_FREE_BYTES set above any plausible free-space figure
/// → writes refused with "below water-mark"; reads keep serving;
/// server alive after the refusal.
#[test]
fn disk_watermark_refuses_writes_keeps_reads_keeps_server_alive() {
    let dir = unique_tmpdir("wm");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    // A petabyte is comfortably larger than any laptop / CI runner
    // filesystem. Using u64::MAX risks integer-overflow surprises in
    // the helper; pick a real-but-impossible figure instead.
    let huge = (1_u64 << 50).to_string();
    let (raw, addrs) = spawn_db_wal(&db, &wal, &[("SPG_WAL_MIN_FREE_BYTES", huge)]);
    let _c = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Reads bypass the water-mark: a pure SELECT must succeed.
    let one = select_int(&mut s, "SELECT 1");
    assert_eq!(one, 1, "read-only SELECT must bypass water-mark");

    // Writes get refused with the documented error message.
    let outcome = run_query(&mut s, "CREATE TABLE w (id INT NOT NULL)");
    match outcome {
        Outcome::Error(msg) => {
            assert!(
                msg.contains("below water-mark"),
                "expected `below water-mark` in error, got: {msg}"
            );
            assert!(
                msg.contains("SPG_WAL_MIN_FREE_BYTES"),
                "error must name the env var so operators can correlate: {msg}"
            );
        }
        Outcome::Ok => panic!("CREATE TABLE should have been refused by water-mark"),
    }
    drop(s);

    // Server alive: reconnect, run a fresh SELECT. The error closed
    // our previous conn (handle() returns Err), but the listener
    // itself keeps running.
    let mut s2 =
        TcpStream::connect(&addrs.native).expect("server still listening after water-mark error");
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let again = select_int(&mut s2, "SELECT 2");
    assert_eq!(
        again, 2,
        "server must keep serving reads after water-mark refusal"
    );
    // We don't try_wait on the child directly — `common::ChildGuard`
    // owns it; the second connection succeeding is the liveness proof.
}
