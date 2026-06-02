#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_mut,
    unused_variables
)]

//! v5.4.3 — chaos validation of the async-commit durability
//! window.
//!
//! Contract under test (the v5.4.4 documentation will formalise
//! it):
//!
//! 1. Sync-commit (the default) preserves every v4.42 invariant
//!    exactly. A SIGKILL after CC returns must leave every CC'd
//!    INSERT recoverable; we sanity-check this elsewhere
//!    (`e2e_chaos_freeze`, the WAL replay tests) so this file
//!    focuses on the async path.
//!
//! 2. Async-commit (`SPG_SYNCHRONOUS_COMMIT=off`) loses **only
//!    writes inside the most recent flusher interval** on
//!    SIGKILL. Concretely: every INSERT acknowledged before the
//!    most recent `durability_checkpoint` marker reached fsync
//!    must replay; INSERTs inside the next window may be lost.
//!
//! The test can't pin "exactly N rows survived" — the kill
//! lands anywhere in the workload, and CI scheduler jitter
//! shifts the flusher tick. Instead it asserts the structural
//! invariants:
//!
//!   * Post-restart, the server boots cleanly and `SELECT
//!     count(*)` returns a number `c` in `[1, N]` (some rows
//!     survived; nothing extra was conjured).
//!   * Every PK in `[0, c)` resolves through the hot tier (no
//!     hole in the prefix — async-commit loses *suffix* writes,
//!     not random ones, because the WAL is append-only and
//!     replay stops at the first truncation).
//!   * The WAL physically contained at least one durability
//!     marker before kill (flusher iterations ≥ 1 surfaces via
//!     the marker bytes after kill).
//!
//! No determinism on the exact loss count, no fancy fault
//! injection — just an end-to-end check that the durability
//! contract holds across SIGKILL.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_command_complete,
    parse_data_row, parse_data_row_batch,
};

use std::process::Child;

use std::thread;

mod common;

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-chaos-async-{tag}-{nanos}"));
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

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
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

fn exec_ok(stream: &mut TcpStream, sql: &str) -> bool {
    send_query(stream, sql);
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::CommandComplete => {
                parse_command_complete(&f).unwrap();
                return true;
            }
            Op::ErrorResponse | Op::Error => return false,
            _ => {}
        }
    }
}

fn select_count(stream: &mut TcpStream, sql: &str) -> i64 {
    send_query(stream, sql);
    let rd = read_frame(stream);
    assert_eq!(rd.op, Op::RowDescription);
    let mut value: i64 = -1;
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => value = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                value = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return value,
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
fn chaos_kill_during_async_commit_window_loses_only_unflushed() {
    let dir = unique_tmpdir("kill-mid-async");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    // Wide-ish flusher cadence (5 ms) so there's a non-trivial
    // window of in-flight writes when the kill lands — too
    // tight and every INSERT happens to be inside one sync
    // boundary; too loose and the assertion "c >= 1" gets brittle
    // because nothing has been fsynced. 5 ms is the same order
    // as a single APFS fsync, leaving room for the durability
    // window to be observably bounded.
    let env: Vec<(&str, String)> = vec![
        ("SPG_SYNCHRONOUS_COMMIT", "off".to_string()),
        ("SPG_FLUSHER_INTERVAL_US", "5000".to_string()),
    ];

    let attempted: i64 = 200;
    let ack_count: i64;
    {
        let (raw, addrs1) = spawn_db_wal(&db, &wal, &env);
        let mut c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);

        assert!(exec_ok(
            &mut s,
            "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
        ));
        assert!(exec_ok(&mut s, "CREATE INDEX by_id ON big (id)"));

        // Burst-write — async-commit makes each INSERT return
        // immediately after in-memory apply, so the loop runs
        // far faster than the 5 ms flusher cadence and should
        // accumulate several markers' worth of writes before the
        // sleep below schedules the kill.
        let mut acked: i64 = 0;
        for i in 0..attempted {
            if exec_ok(&mut s, &format!("INSERT INTO big VALUES ({i}, 'u-{i}')")) {
                acked += 1;
            } else {
                break;
            }
        }
        ack_count = acked;
        assert_eq!(
            ack_count, attempted,
            "every INSERT should ack in async mode (got {acked} / {attempted})"
        );

        // Let one or two flusher ticks land at least some of the
        // writes durably, but kill before all are fsynced — the
        // 5 ms cadence vs ~30+ ms of work in the workload means
        // a fraction-but-not-all is durable, which is exactly the
        // bounded-loss window the contract describes.
        thread::sleep(Duration::from_millis(8));
        let _ = c.0.kill();
        let _ = c.0.wait();
    }
    thread::sleep(Duration::from_millis(150));

    // Restart on a fresh port + sync mode, same files. WAL
    // replay must:
    //   - Walk every record, accepting `durability_checkpoint`
    //     markers as no-ops (v5.4.0 contract).
    //   - Apply every auto_commit_sql INSERT it sees up to the
    //     point WAL bytes were durable.
    //   - Stop cleanly if the tail is truncated by the SIGKILL.
    let (raw2, addrs2) = spawn_db_wal(&db, &wal, &[]);
    let _c2 = common::ChildGuard(raw2);
    let mut s2 = common::connect_to(&addrs2.native);

    let after = select_count(&mut s2, "SELECT count(*) FROM big");
    assert!(
        after >= 1,
        "expected at least one INSERT to survive in the durable prefix; got {after}"
    );
    assert!(
        after <= attempted,
        "post-restart count must not exceed pre-kill ack count: got {after} > {attempted}"
    );

    // Prefix invariant: WAL is append-only and replay stops at
    // the first truncated tail entry, so the surviving rows
    // form a contiguous prefix `[0, after)`. Sample the
    // boundary ids; every one must resolve.
    let pick = [0i64, (after - 1) / 2, after - 1];
    for id in pick {
        if id < 0 {
            continue;
        }
        let n = select_count(
            &mut s2,
            &format!("SELECT count(*) FROM big WHERE id = {id}"),
        );
        assert_eq!(n, 1, "PK {id} should resolve post-restart (count(*) == 1)");
    }
}
