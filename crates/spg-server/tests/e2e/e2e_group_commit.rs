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

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use std::thread;

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-gc-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

    {
        let (raw, addrs1) = local_spawn(&db, &wal, &[]);
        let _c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);
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
    // v7.37 (round 827) — no sleep: ChildGuard::drop above did
    // kill+wait, and wait() IS the event (process reaped, files free).
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let _c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
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

    {
        let (raw, addrs1) = local_spawn(&db, &wal, &[]);
        let _c = common::ChildGuard(raw);
        let mut setup = common::connect_to(&addrs1.native);
        setup.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(
            &mut setup,
            "CREATE TABLE m (tid INT NOT NULL, i INT NOT NULL)",
        );
        drop(setup);

        let server_addr = addrs1.native.clone();
        let succeeded = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let addr = server_addr.clone();
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

        let mut probe = TcpStream::connect(&addrs1.native).expect("connect for SELECT");
        probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let live = select_int(&mut probe, "SELECT count(*) FROM m");
        assert_eq!(
            live, total,
            "expected {total} rows after 4-way concurrent insert, got {live}"
        );
    }

    // Durability across restart — replay should yield the same total.
    // v7.37 (round 827) — no sleep: ChildGuard::drop did kill+wait.
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let _c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let restored = select_int(&mut s2, "SELECT count(*) FROM m");
    assert_eq!(
        restored, total,
        "expected {total} rows after multi-client group-commit restart, got {restored}"
    );
}
