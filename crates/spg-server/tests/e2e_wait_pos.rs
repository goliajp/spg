#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.1.7 — `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]`.
//!
//! Server-layer command — engine refuses it; spg-server's
//! dispatch intercepts before it reaches the engine and polls
//! `lag_state.follower_applied_pos` until the target is reached
//! or the timeout fires.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-wait-e2e-{tag}-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_with_repl(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl()
        .spawn()
}

fn spawn_follower(
    db: &std::path::Path,
    wal: &std::path::Path,
    follow_of: &str,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .env("SPG_FOLLOW_OF", follow_of)
        .spawn()
}

fn wait_for_addr(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok_count(s: &mut TcpStream, sql: &str) -> u64 {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return parse_command_complete(&f).unwrap(),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected SQL {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    exec_ok_count(s, sql);
}

#[test]
fn wait_for_position_zero_returns_immediately() {
    // On any server, the apply position starts at 0. A
    // `WAIT FOR WAL POSITION 0` should return reached=1
    // without any sleep.
    let dir = unique_tmpdir("zero");
    let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let t0 = Instant::now();
    let reached = exec_ok_count(&mut client, "WAIT FOR WAL POSITION 0");
    let elapsed = t0.elapsed();
    assert_eq!(reached, 1, "WAIT FOR WAL POSITION 0 must always reach");
    assert!(
        elapsed < Duration::from_millis(50),
        "expected immediate return, took {:?}",
        elapsed
    );
}

#[test]
fn wait_for_position_timeout_returns_zero() {
    // No follower path, no master to apply WAL from. The local
    // server's apply position will never reach a large target —
    // the timeout fires and we get reached=0.
    let dir = unique_tmpdir("tmo");
    let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let t0 = Instant::now();
    let reached = exec_ok_count(
        &mut client,
        "WAIT FOR WAL POSITION 999999999 WITH TIMEOUT 300",
    );
    let elapsed = t0.elapsed();
    assert_eq!(reached, 0, "timeout case must report reached=0");
    assert!(
        elapsed >= Duration::from_millis(280),
        "expected ≥ 280ms wait, took {:?}",
        elapsed
    );
    assert!(
        elapsed < Duration::from_millis(1000),
        "expected ≤ 1s wait, took {:?}",
        elapsed
    );
}

#[test]
fn wait_for_position_resolves_when_follower_catches_up() {
    // Master writes records into its WAL; a follower receives
    // them and advances `follower_applied_pos`. A
    // `WAIT FOR WAL POSITION <target>` on the follower returns
    // reached=1 within the timeout.
    let dir_m = unique_tmpdir("master");
    let dir_f = unique_tmpdir("follower");

    let (m_raw, m_addrs) = spawn_with_repl(&dir_m.join("m.db"), &dir_m.join("m.wal"));
    let _m_guard = common::ChildGuard(m_raw);
    let mut m_client = common::connect_to(&m_addrs.native);
    m_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(m_addrs.repl.as_ref().unwrap());

    let (f_raw, f_addrs) = spawn_follower(
        &dir_f.join("f.db"),
        &dir_f.join("f.wal"),
        m_addrs.repl.as_ref().unwrap(),
    );
    let _f_guard = common::ChildGuard(f_raw);
    let mut f_client = common::connect_to(&f_addrs.native);
    f_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Drive a few records so the master's WAL grows + the
    // follower's apply position advances. Each INSERT is one
    // auto-commit record; total byte count grows monotonically.
    exec_ok(&mut m_client, "CREATE TABLE t (id INT NOT NULL)");
    for i in 0..10 {
        exec_ok(&mut m_client, &format!("INSERT INTO t VALUES ({i})"));
    }
    // Give the follower a beat to apply.
    std::thread::sleep(Duration::from_millis(400));

    // Read the follower's current applied_pos via SHOW
    // SUBSCRIPTIONS? No — we want the FOLLOWER's apply pos,
    // not a subscription's. The native wire has no exposed
    // accessor for that; pick a small target we're confident
    // the follower has crossed.
    let target: u64 = 50; // far less than 10 INSERTs would produce
    let t0 = Instant::now();
    let reached = exec_ok_count(
        &mut f_client,
        &format!("WAIT FOR WAL POSITION {target} WITH TIMEOUT 5000"),
    );
    let elapsed = t0.elapsed();
    assert_eq!(reached, 1, "follower must have crossed position {target}");
    assert!(
        elapsed < Duration::from_millis(200),
        "expected near-instant return (already crossed), took {:?}",
        elapsed
    );
}

#[test]
fn wait_for_resolves_after_target_is_reached() {
    // Drive the follower from BEHIND a target. The wait blocks,
    // then the master writes more records, the follower applies,
    // and the wait resolves before timeout.
    let dir_m = unique_tmpdir("master2");
    let dir_f = unique_tmpdir("follower2");

    let (m_raw, m_addrs) = spawn_with_repl(&dir_m.join("m.db"), &dir_m.join("m.wal"));
    let _m_guard = common::ChildGuard(m_raw);
    let mut m_client = common::connect_to(&m_addrs.native);
    m_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(m_addrs.repl.as_ref().unwrap());

    let (f_raw, f_addrs) = spawn_follower(
        &dir_f.join("f.db"),
        &dir_f.join("f.wal"),
        m_addrs.repl.as_ref().unwrap(),
    );
    let _f_guard = common::ChildGuard(f_raw);
    let mut f_client = common::connect_to(&f_addrs.native);
    f_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut m_client, "CREATE TABLE t (id INT NOT NULL)");
    std::thread::sleep(Duration::from_millis(300));

    // Pick a target high enough that the follower hasn't crossed
    // yet, but low enough that 5 more INSERTs will push past.
    let target: u64 = 500;
    // Spawn a thread that writes after a short delay, simulating
    // the master making progress while the follower's wait is
    // blocking.
    let m_native = m_addrs.native.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let mut s = common::connect_to(&m_native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        for i in 0..50 {
            exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
    });

    let t0 = Instant::now();
    let reached = exec_ok_count(
        &mut f_client,
        &format!("WAIT FOR WAL POSITION {target} WITH TIMEOUT 5000"),
    );
    let elapsed = t0.elapsed();
    writer.join().unwrap();
    assert_eq!(
        reached, 1,
        "follower must catch up to position {target} within 5s"
    );
    assert!(
        elapsed < Duration::from_millis(5000),
        "expected resolve under 5s, took {:?}",
        elapsed
    );
}

#[test]
fn wait_for_no_timeout_with_zero_target_does_not_block() {
    // Bare `WAIT FOR WAL POSITION 0` (no timeout clause) on a
    // server whose apply pos is already 0 must NOT block forever
    // — the 0 ≥ 0 check fires the first time around the poll.
    let dir = unique_tmpdir("notmo");
    let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let t0 = Instant::now();
    let reached = exec_ok_count(&mut client, "WAIT FOR WAL POSITION 0");
    let elapsed = t0.elapsed();
    assert_eq!(reached, 1);
    assert!(elapsed < Duration::from_millis(50));
}
