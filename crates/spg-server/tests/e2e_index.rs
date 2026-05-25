//! B-tree index end-to-end:
//! - CREATE INDEX over the wire registers the index server-side.
//! - SELECT WHERE col = X returns the same row(s) as a full-scan would.
//! - After a daemon restart with a persisted db, the index is rebuilt from
//!   the restored rows (index definitions are persisted, data is rebuilt).

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_command_complete, parse_data_row};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-idx-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn spawn_server(addr: &str, db: Option<&PathBuf>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr);
    if let Some(d) = db {
        cmd.arg(d);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn spg-server")
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
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
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

fn run_select(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::CommandComplete => return rows,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

fn populate_and_index(stream: &mut TcpStream) {
    send_query(
        stream,
        "CREATE TABLE accounts (id INT NOT NULL, name TEXT NOT NULL, balance FLOAT)",
    );
    expect_cc(stream);
    for i in 1..=5 {
        let q = format!("INSERT INTO accounts VALUES ({i}, 'user{i}', {i}.5)");
        send_query(stream, &q);
        expect_cc(stream);
    }
    send_query(stream, "CREATE INDEX by_id ON accounts (id)");
    expect_cc(stream);
}

#[test]
fn select_eq_returns_correct_row_via_index() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, None));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    populate_and_index(&mut s);

    let rows = run_select(&mut s, "SELECT * FROM accounts WHERE id = 3");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], WireValue::Int(3));
    assert_eq!(rows[0][1], WireValue::Text("user3".into()));

    let none = run_select(&mut s, "SELECT * FROM accounts WHERE id = 99");
    assert!(none.is_empty());
}

#[test]
fn index_seek_and_full_scan_return_same_rows() {
    // Build two daemons: same data, one with index, one without. Same WHERE
    // → same row set.
    let addr_a = pick_free_addr();
    let mut child_a = ChildGuard(spawn_server(&addr_a, None));
    let mut sa = wait_for_listener(&addr_a, &mut child_a.0);
    sa.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(
        &mut sa,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );
    expect_cc(&mut sa);
    for i in 1..=5 {
        send_query(&mut sa, &format!("INSERT INTO t VALUES ({i}, 'n{i}')"));
        expect_cc(&mut sa);
    }
    let scan_rows = run_select(&mut sa, "SELECT * FROM t WHERE id = 4");

    let addr_b = pick_free_addr();
    let mut child_b = ChildGuard(spawn_server(&addr_b, None));
    let mut sb = wait_for_listener(&addr_b, &mut child_b.0);
    sb.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(
        &mut sb,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );
    expect_cc(&mut sb);
    for i in 1..=5 {
        send_query(&mut sb, &format!("INSERT INTO t VALUES ({i}, 'n{i}')"));
        expect_cc(&mut sb);
    }
    send_query(&mut sb, "CREATE INDEX ix ON t (id)");
    expect_cc(&mut sb);
    let idx_rows = run_select(&mut sb, "SELECT * FROM t WHERE id = 4");

    assert_eq!(scan_rows, idx_rows);
    assert_eq!(idx_rows.len(), 1);
    assert_eq!(idx_rows[0][1], WireValue::Text("n4".into()));
}

#[test]
fn index_definition_survives_daemon_restart() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    // Phase 1: build the schema + populate + create the index.
    {
        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn_server(&addr, Some(&db)));
        let mut s = wait_for_listener(&addr, &mut child.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        populate_and_index(&mut s);
    }

    // Phase 2: restart and verify SELECT WHERE id = 4 still hits exactly one
    // row — the index was rebuilt from the rows on startup.
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, Some(&db)));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let rows = run_select(&mut s, "SELECT * FROM accounts WHERE id = 4");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], WireValue::Text("user4".into()));

    fs::remove_dir_all(&dir).ok();
}
