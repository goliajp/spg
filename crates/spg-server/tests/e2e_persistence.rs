//! Cross-process persistence:
//! spawn the daemon with a db-path arg → CREATE/INSERT via wire → kill →
//! spawn a fresh daemon on the same db-path → verify SELECT sees the rows.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{
    Frame, Op, WireValue, build_query, encode, parse_command_complete, parse_data_row,
    parse_row_description,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_server(addr: &str, db: &PathBuf) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .arg(db)
        .stdout(Stdio::null())
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

#[test]
fn rows_survive_a_full_daemon_restart() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    // Phase 1: fresh daemon, populate, kill.
    {
        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn_server(&addr, &db));
        let mut stream = wait_for_listener(&addr, &mut child.0);
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        send_query(
            &mut stream,
            "CREATE TABLE accounts (id INT NOT NULL, name TEXT NOT NULL, balance FLOAT)",
        );
        expect_cc(&mut stream);
        send_query(
            &mut stream,
            "INSERT INTO accounts VALUES (1, 'alice', 100.5)",
        );
        expect_cc(&mut stream);
        send_query(&mut stream, "INSERT INTO accounts VALUES (2, 'bob', NULL)");
        expect_cc(&mut stream);
        send_query(&mut stream, "INSERT INTO accounts VALUES (3, 'cara', 42.0)");
        expect_cc(&mut stream);
        // Drop ChildGuard => server killed, db file should already be flushed.
    }

    assert!(db.exists(), "db file should exist after first run");

    // Phase 2: fresh daemon, restored from db, query.
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &db));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut stream, "SELECT * FROM accounts");
    let rd = read_frame(&mut stream);
    assert_eq!(rd.op, Op::RowDescription);
    let cols = parse_row_description(&rd).unwrap();
    assert_eq!(cols.len(), 3);

    let mut rows = Vec::new();
    loop {
        let f = read_frame(&mut stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], WireValue::Text("alice".into()));
    assert_eq!(rows[1][1], WireValue::Text("bob".into()));
    assert_eq!(rows[1][2], WireValue::Null);
    assert_eq!(rows[2][2], WireValue::Float(42.0));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_db_file_starts_fresh_and_creates_it() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    assert!(!db.exists(), "db should NOT exist at start");

    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &db));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut stream, "CREATE TABLE t (v INT)");
    expect_cc(&mut stream);
    send_query(&mut stream, "INSERT INTO t VALUES (1)");
    expect_cc(&mut stream);
    assert!(db.exists(), "db file should now exist");
    drop(child);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_only_select_does_not_touch_the_db_file() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &db));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut stream, "CREATE TABLE t (v INT)");
    expect_cc(&mut stream);
    let bytes_before = fs::read(&db).unwrap();

    send_query(&mut stream, "SELECT * FROM t");
    // Drain the RowDescription + CommandComplete (no DataRow since empty).
    assert_eq!(read_frame(&mut stream).op, Op::RowDescription);
    assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);

    let bytes_after = fs::read(&db).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "SELECT-only should not rewrite the db file"
    );

    fs::remove_dir_all(&dir).ok();
}
