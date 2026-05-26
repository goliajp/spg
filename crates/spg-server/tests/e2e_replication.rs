#![allow(
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args
)]

//! v4.24 single-master / multi-follower WAL streaming replication.
//!
//! Spins up two spg-server processes — one master with replication
//! enabled, one follower pointed at it — and verifies that writes
//! made on the master are visible on the follower.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(10);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-repl-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn(addr: &str, db: &PathBuf, wal: &PathBuf, extra_env: Vec<(&str, String)>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-") // audit path off
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in extra_env {
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

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    let f = read_frame(s);
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

fn select_count(s: &mut TcpStream, sql: &str) -> i64 {
    select_count_opt(s, sql).expect("expected scalar result")
}

/// Send a SELECT and return the first cell as i64, or None if the
/// server returned an ErrorResponse. Drains frames until
/// CommandComplete / ErrorResponse so the connection stays clean.
fn select_count_opt(s: &mut TcpStream, sql: &str) -> Option<i64> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        return None;
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
            Op::CommandComplete => return Some(count),
            Op::ErrorResponse => return None,
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

#[test]
fn follower_bootstraps_from_master_snapshot_and_tails_writes() {
    let master_addr = pick_free_addr();
    let repl_addr = pick_free_addr();
    let follower_addr = pick_free_addr();

    let dir = unique_tmpdir();
    let master_db = dir.join("master.db");
    let master_wal = dir.join("master.wal");
    let follower_db = dir.join("follower.db");
    let follower_wal = dir.join("follower.wal");

    let mut master = ChildGuard(spawn(
        &master_addr,
        &master_db,
        &master_wal,
        vec![("SPG_REPL_ADDR", repl_addr.clone())],
    ));
    let mut ms = wait_for_listener(&master_addr, &mut master.0);
    ms.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Pre-bootstrap data on master so the snapshot already has rows.
    exec_ok(
        &mut ms,
        "CREATE TABLE rep (id INT NOT NULL, v INT NOT NULL)",
    );
    for i in 1..=3 {
        exec_ok(
            &mut ms,
            &format!("INSERT INTO rep VALUES ({i}, {})", i * 10),
        );
    }
    assert_eq!(select_count(&mut ms, "SELECT count(*) FROM rep"), 3);

    // Start follower. It will connect to master, fetch snapshot, then tail.
    let mut follower = ChildGuard(spawn(
        &follower_addr,
        &follower_db,
        &follower_wal,
        vec![("SPG_FOLLOW_OF", repl_addr.clone())],
    ));
    let mut fs = wait_for_listener(&follower_addr, &mut follower.0);
    fs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Poll follower until snapshot is applied (table appears AND
    // count matches). Before the snapshot arrives the table doesn't
    // exist so we get an ErrorResponse — drained cleanly.
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        if let Some(3) = select_count_opt(&mut fs, "SELECT count(*) FROM rep") {
            break;
        }
        if Instant::now() > deadline {
            panic!("follower did not catch up to snapshot");
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Now write more on master and verify follower tails the new
    // records (not in snapshot).
    exec_ok(&mut ms, "INSERT INTO rep VALUES (4, 40)");
    exec_ok(&mut ms, "INSERT INTO rep VALUES (5, 50)");

    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        let got = select_count(&mut fs, "SELECT count(*) FROM rep");
        if got == 5 {
            break;
        }
        if Instant::now() > deadline {
            panic!("follower never caught up to streamed writes (last seen {got})");
        }
        thread::sleep(Duration::from_millis(100));
    }
}
