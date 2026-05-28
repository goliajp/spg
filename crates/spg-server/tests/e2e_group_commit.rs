#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.42 group-commit correctness suite. The fsync coalescing
//! design has two structural invariants the rest of the suite
//! doesn't otherwise pin:
//!
//! 1. **Group of 1 = no latency tax.** A single client with no
//!    contention must walk straight through the leader path
//!    (push → become leader → drain a group of 1 → fsync →
//!    install → ack) without ever blocking on the queue. Slow-
//!    path checks live in `slo_smoke::slo_wal_insert_p99_under_budget`;
//!    here we just verify the *behaviour* — 100 sequential
//!    INSERTs all return CC, the count matches, and the writes
//!    survive a restart.
//!
//! 2. **N concurrent writers see consistent fan-out.** 4 client
//!    threads each push their own INSERTs; the elected leader
//!    drains them into groups, fsyncs the batch once, and acks
//!    every survivor. The total durable row count must match
//!    `4 × N` after restart (no phantoms, no missing rows, no
//!    interleaving corruption from the multi-slot engine map).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
    let p = std::env::temp_dir().join(format!("spg-gc-{tag}-{nanos}"));
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

fn exec_ok(s: &mut TcpStream, sql: &str) {
    assert_eq!(run_query(s, sql), Outcome::Ok, "expected ok for {sql:?}");
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

// ---- group of 1 ----

/// One client, 100 sequential auto-commit INSERTs. With v4.42 each
/// INSERT goes through the commit-barrier queue, but with no
/// concurrent writers every push immediately finds `leader_active
/// = false`, becomes the leader for a group of 1, drains, fsyncs,
/// installs, and acks itself. No follower wait, no condvar
/// spuriousness — the path should look identical to v4.41.1 from
/// the wire side.
#[test]
fn single_client_group_of_one_no_latency_tax() {
    let dir = unique_tmpdir("g1");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr1 = pick_free_addr();

    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, &wal, &[]));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE g (id INT NOT NULL)");
        for i in 0..100 {
            assert_eq!(
                run_query(&mut s, &format!("INSERT INTO g VALUES ({i})")),
                Outcome::Ok,
                "INSERT {i} failed unexpectedly under group-of-1 path",
            );
        }
        let count = select_int(&mut s, "SELECT count(*) FROM g");
        assert_eq!(count, 100, "expected 100 rows after sequential inserts");
    }

    // Restart and confirm durability — every CC must replay.
    thread::sleep(Duration::from_millis(150));
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let restored = select_int(&mut s2, "SELECT count(*) FROM g");
    assert_eq!(
        restored, 100,
        "expected 100 rows after group-of-1 restart, got {restored}"
    );
}

// ---- 4 concurrent writers ----

/// Four client threads each issue 25 INSERTs against the same
/// table. The commit-barrier leader drains the queue into groups
/// (rolling drain up to `SPG_COMMIT_GROUP_MAX` per group), shares
/// one fsync per group, and acks every survivor. No two writers
/// may corrupt the shared `tx_catalogs` map; no row may be lost
/// or duplicated. Total `4 × 25 = 100` rows must be visible and
/// durable across restart.
#[test]
fn four_client_concurrent_inserts_all_durable() {
    const THREADS: usize = 4;
    const PER_THREAD: i64 = 25;
    let total: i64 = i64::try_from(THREADS).unwrap() * PER_THREAD;

    let dir = unique_tmpdir("g4");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr1 = pick_free_addr();

    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, &wal, &[]));
        let mut setup = wait_for_listener(&addr1, &mut c.0);
        setup.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(
            &mut setup,
            "CREATE TABLE m (tid INT NOT NULL, i INT NOT NULL)",
        );
        drop(setup);

        let succeeded = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let addr = addr1.clone();
            let succeeded = Arc::clone(&succeeded);
            handles.push(thread::spawn(move || {
                let mut s = TcpStream::connect(&addr).expect("connect");
                s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
                let mut local_ok = 0usize;
                for i in 0..PER_THREAD {
                    if run_query(&mut s, &format!("INSERT INTO m VALUES ({t}, {i})")) == Outcome::Ok
                    {
                        local_ok += 1;
                    } else {
                        panic!("thread {t} INSERT {i} failed");
                    }
                }
                succeeded.fetch_add(local_ok, Ordering::Relaxed);
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(
            i64::try_from(succeeded.load(Ordering::Relaxed)).unwrap(),
            total,
            "every CC'd insert from every thread must have stuck",
        );

        let mut probe = TcpStream::connect(&addr1).expect("connect for SELECT");
        probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let live = select_int(&mut probe, "SELECT count(*) FROM m");
        assert_eq!(
            live, total,
            "expected {total} rows after 4-way concurrent insert, got {live}"
        );
    }

    // Durability across restart — replay should yield the same total.
    thread::sleep(Duration::from_millis(150));
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let restored = select_int(&mut s2, "SELECT count(*) FROM m");
    assert_eq!(
        restored, total,
        "expected {total} rows after multi-client group-commit restart, got {restored}"
    );
}
