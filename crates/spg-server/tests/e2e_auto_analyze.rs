#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.2.1 — background auto-analyze worker e2e.
//!
//! Sets `SPG_AUTO_ANALYZE_INTERVAL_MS=200` so the worker sweeps
//! within a fast test window. Drives DML through the wire and
//! observes `spg_statistic` populating without an explicit
//! ANALYZE.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-auto-an-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_with_interval(
    db: &std::path::Path,
    interval_ms: u64,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .env("SPG_AUTO_ANALYZE_INTERVAL_MS", interval_ms.to_string())
        .spawn()
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
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription, "got {:?}", rd.op);
    let mut last: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => {
                last = match &parse_data_row(&f).unwrap()[0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                last = match &rows[0][0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::CommandComplete => return last,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn count_spg_statistic_rows(s: &mut TcpStream) -> usize {
    // The virtual-table short-circuit only matches SELECT * (no
    // aggregates), so we read all rows and count client-side.
    send(s, &build_query("SELECT * FROM spg_statistic"));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription, "got {:?}", rd.op);
    let mut n = 0usize;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => n += 1,
            Op::DataRowBatch => n += parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => return n,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wait_for_spg_statistic_rows(addr: &str, target: usize, deadline: Instant) -> usize {
    loop {
        if let Ok(mut s) = TcpStream::connect(addr) {
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let got = count_spg_statistic_rows(&mut s);
            if got >= target {
                return got;
            }
            if Instant::now() >= deadline {
                return got;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn sweep_fires_after_10pct_threshold() {
    let dir = unique_tmpdir();
    let (raw, addrs) = spawn_with_interval(&dir.join("s.db"), 200);
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    // 10+ INSERTs on a fresh table → threshold = 0.1 × max(N, 100) = 10
    // (after 10 inserts, row_count=10 → threshold = 10). The worker
    // sweep should pick this up within ~400 ms (sweep interval 200 ms
    // + ANALYZE cycle).
    for i in 0..10 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }

    let got = wait_for_spg_statistic_rows(&addrs.native, 1, Instant::now() + PROBE_TIMEOUT);
    assert_eq!(got, 1, "auto-analyze must populate spg_statistic");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_sweep_when_under_threshold() {
    let dir = unique_tmpdir();
    let (raw, addrs) = spawn_with_interval(&dir.join("s.db"), 200);
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    // 5 inserts is below the threshold of 10 for tiny tables.
    for i in 0..5 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    // Sleep through several sweep intervals to prove no auto-ANALYZE
    // ran. 1 s = 5 sweep ticks.
    std::thread::sleep(Duration::from_millis(1000));
    let n = count_spg_statistic_rows(&mut s);
    assert_eq!(n, 0, "spg_statistic must stay empty under threshold");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn sweep_concurrent_with_reads_does_not_block() {
    let dir = unique_tmpdir();
    let (raw, addrs) = spawn_with_interval(&dir.join("s.db"), 200);
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)",
    );
    for i in 0..20 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, 'x')"));
    }
    // While the worker is sweeping (and intermittently taking the
    // write-lock), this client's read queries must still return
    // within READ_TIMEOUT. Run 30 reads spaced 50 ms — total 1.5 s,
    // crossing multiple sweep windows.
    let t0 = Instant::now();
    for _ in 0..30 {
        let n = select_int(&mut s, "SELECT count(*) FROM t");
        assert_eq!(n, 20);
        std::thread::sleep(Duration::from_millis(50));
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "reads got starved by the auto-analyze worker: took {:?}",
        elapsed
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn interval_zero_disables_worker() {
    let dir = unique_tmpdir();
    // SPG_AUTO_ANALYZE_INTERVAL_MS=0 should opt the worker out
    // entirely. Without the worker, the spg_statistic stays empty
    // until an explicit ANALYZE.
    let (raw, addrs) = spawn_with_interval(&dir.join("s.db"), 0);
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 0..30 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(count_spg_statistic_rows(&mut s), 0);

    fs::remove_dir_all(&dir).ok();
}
