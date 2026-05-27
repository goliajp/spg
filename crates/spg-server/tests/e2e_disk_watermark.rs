#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

//! v4.33 disk water-mark — `SPG_WAL_MIN_FREE_BYTES`. When the WAL
//! volume's free space is below the threshold, writes are refused
//! with a clear error; reads keep serving; the server stays alive.
//!
//! Test strategy: set the threshold to a value larger than any
//! real filesystem free space (u64::MAX / 2), so the water-mark is
//! guaranteed to trigger on every write attempt. Assert reads still
//! succeed and the server stays responsive.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-watermark-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(addr: &str, db: &Path, wal: &Path, env: &[(&str, String)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
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
    let addr = pick_free_addr();
    let dir = unique_tmpdir("wm");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    // A petabyte is comfortably larger than any laptop / CI runner
    // filesystem. Using u64::MAX risks integer-overflow surprises in
    // the helper; pick a real-but-impossible figure instead.
    let huge = (1_u64 << 50).to_string();
    let mut c = ChildGuard(spawn_server(
        &addr,
        &db,
        &wal,
        &[("SPG_WAL_MIN_FREE_BYTES", huge)],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
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
    let mut s2 = TcpStream::connect(&addr).expect("server still listening after water-mark error");
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let again = select_int(&mut s2, "SELECT 2");
    assert_eq!(
        again, 2,
        "server must keep serving reads after water-mark refusal"
    );

    // And the server didn't crash — try_wait reports it as running.
    assert!(
        c.0.try_wait().expect("try_wait").is_none(),
        "server must not have exited after water-mark refusal"
    );
    let _ = Instant::now(); // suppress unused-import warning on Instant in some configs
}
