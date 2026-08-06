//! v6.7.4 ship-gate #1 — `workload_completes_under_sustained_writes`.
//!
//! Drives a sustained-write workload against an spg-server child
//! with `SPG_FREEZER_WORKERS=4`, a tight hot-tier budget so the
//! freezer fires on most ticks, and a small batch so the freezer
//! has lots of work to do. The gate enforces:
//!
//!   1. The workload completes inside a sane wall-clock budget —
//!      the parallel freezer must not stall foreground writes.
//!   2. Every inserted PK resolves post-workload (cold + hot
//!      tier combined). The k-way merge in
//!      `Catalog::commit_freeze_slices` is the new code path; a
//!      regression there would manifest as missing rows.
//!   3. At least two cold segments landed (proves the freezer
//!      actually fired with workers > 1 during the run).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_data_row_batch, parse_error_response};

use crate::common::{ChildGuard, ServerBuilder};

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WORKLOAD_BUDGET: Duration = Duration::from_secs(60);

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_until_cc(s: &mut TcpStream, sql: &str) {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "SQL failed: {sql:?} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc(s, sql);
}

fn count_rows(s: &mut TcpStream, sql: &str) -> usize {
    send_query(s, sql);
    let mut total = 0usize;
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::DataRow => total += 1,
            Op::DataRowBatch => {
                let f = spg_wire::Frame { op, payload: body };
                if let Ok(rows) = parse_data_row_batch(&f) {
                    total += rows.len();
                }
            }
            Op::CommandComplete => return total,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "select failed: {sql} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

#[test]
fn workload_completes_under_sustained_writes() {
    let (raw, addrs) = ServerBuilder::new()
        .env("SPG_HOT_TIER_BYTES", "256")
        .env("SPG_FREEZER_TICK_MS", "50")
        .env("SPG_FREEZER_BATCH_ROWS", "16")
        // The v6.7.4 hot path: workers > 1 forces the parallel
        // slice/commit code.
        .env("SPG_FREEZER_WORKERS", "4")
        .spawn();
    let _guard = ChildGuard(raw);
    let mut s = TcpStream::connect(&addrs.native).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL, payload TEXT)");
    exec_native(&mut s, "CREATE INDEX by_id ON t (id)");
    let n = 200i64;
    let start = Instant::now();
    for i in 0..n {
        // Payload long enough that hot_tier_bytes=256 fills quickly.
        exec_native(
            &mut s,
            &format!("INSERT INTO t VALUES ({i}, 'aaaaaaaaaaaaaa-row-{i}')"),
        );
    }
    let workload_wall = start.elapsed();
    assert!(
        workload_wall < WORKLOAD_BUDGET,
        "200-row workload took {workload_wall:?} (budget {WORKLOAD_BUDGET:?})"
    );

    // v7.39 (round 784) — wait for the cold-segment counter to reach
    // the assertion's floor instead of sleeping a fixed drain. The
    // assertion below is unchanged; this only stops a loaded box from
    // reading the counter mid-tick (round 783's flake class).

    // Sanity: at least two cold segments landed — proves the
    // parallel freezer actually ran with workers > 1 (a
    // single-threaded path would have produced the same count, so
    // this gate is about presence, not parallelism).
    let mut cold_segs = 0;
    crate::common::wait_until(Duration::from_secs(20), || {
        cold_segs = count_rows(&mut s, "SELECT * FROM spg_stat_segment");
        cold_segs >= 2
    });
    assert!(
        cold_segs >= 2,
        "expected ≥ 2 cold segments after sustained writes, got {cold_segs}"
    );

    // Every inserted PK must resolve via PK seek (some via hot,
    // some via cold).
    let mut missing = Vec::new();
    for id in 0..n {
        let cnt = count_rows(&mut s, &format!("SELECT id FROM t WHERE id = {id}"));
        if cnt != 1 {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "{} PKs disappeared after parallel-freezer workload: {:?}",
        missing.len(),
        &missing[..missing.len().min(10)]
    );
}
