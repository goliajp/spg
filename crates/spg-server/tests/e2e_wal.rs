//! WAL end-to-end:
//! - Basic restart: writes go into the WAL, a fresh daemon replays them.
//! - Transaction cycle: BEGIN/INSERTs/COMMIT survive a restart via WAL.
//! - Partial transaction at end of WAL (simulated by hand-appending entries
//!   without a COMMIT) is auto-rolled-back on startup.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

// WAL append fsyncs every entry, so when cargo test runs many integration
// binaries in parallel, fsync contention on the temp filesystem can push
// per-query latency well past the 3 s we'd otherwise tolerate. Generous
// timeouts here just paper over that for tests; production durability is
// still per-entry fsync.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-wal-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn spawn_server_wal(addr: &str, db: &Path, wal: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .arg(db)
        .arg("-") // no audit
        .arg(wal)
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

fn count_select_rows(stream: &mut TcpStream, sql: &str) -> usize {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => count += 1,
            Op::CommandComplete => return count,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[test]
fn wal_basic_replay_restores_outside_tx_writes() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: write a bunch of outside-TX statements; kill the daemon.
    {
        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
        let mut s = wait_for_listener(&addr, &mut child.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
        expect_cc(&mut s);
        for i in 1..=3 {
            send_query(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            expect_cc(&mut s);
        }
    }

    // Phase 2: fresh daemon replays from the WAL.
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(count_select_rows(&mut s, "SELECT * FROM t"), 3);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn wal_replay_handles_full_transaction_cycle() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    {
        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
        let mut s = wait_for_listener(&addr, &mut child.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
        expect_cc(&mut s);
        send_query(&mut s, "BEGIN");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (1)");
        expect_cc(&mut s);
        send_query(&mut s, "INSERT INTO t VALUES (2)");
        expect_cc(&mut s);
        send_query(&mut s, "COMMIT");
        expect_cc(&mut s);
    }

    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(count_select_rows(&mut s, "SELECT * FROM t"), 2);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn partial_tx_at_end_of_wal_is_auto_rolled_back() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: CREATE TABLE outside TX (one clean WAL entry).
    {
        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
        let mut s = wait_for_listener(&addr, &mut child.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT NOT NULL)");
        expect_cc(&mut s);
    }

    // Phase 2: hand-append BEGIN + INSERT (no COMMIT) — simulates a server
    // crash mid-transaction with the WAL entries already fsync'd.
    let mut f = OpenOptions::new().append(true).open(&wal).unwrap();
    for sql in ["BEGIN", "INSERT INTO t VALUES (1)"] {
        let len = u32::try_from(sql.len()).unwrap().to_le_bytes();
        f.write_all(&len).unwrap();
        f.write_all(sql.as_bytes()).unwrap();
    }
    drop(f);

    // Phase 3: fresh daemon. Startup must succeed, the table must exist,
    // and the never-committed INSERT must not be visible.
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_wal(&addr, &db, &wal));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(count_select_rows(&mut s, "SELECT * FROM t"), 0);

    fs::remove_dir_all(&dir).ok();
}
