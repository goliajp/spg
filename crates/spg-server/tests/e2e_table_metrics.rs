#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

//! v4.35 per-table metrics — `spg_table_rows{table=...}` +
//! `spg_table_bytes{table=...}` series exposed via `/metrics`,
//! gated by `SPG_METRICS_TABLE_TOPN` (default 50) or, when set, an
//! exact `SPG_METRICS_TABLE_ALLOWLIST`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn spawn_server(addr: &str, http_addr: &str, envs: &[(&str, String)]) -> Child {
    // In-memory only: no db_path / no WAL. The /metrics path reads
    // the live catalog regardless of persistence mode, and skipping
    // WAL fsync keeps the test parallelism-friendly (the suite runs
    // three tests concurrently — three real WAL volumes would slug
    // each other on shared CI disks).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .env_remove("SPG_DB")
        .env_remove("SPG_WAL")
        .env("SPG_HTTP_ADDR", http_addr);
    for (k, v) in envs {
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
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected SQL {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn fetch_metrics(http_addr: &str) -> String {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut s = loop {
        match TcpStream::connect(http_addr) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("/metrics never came up: {e}"),
        }
    };
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

/// Default behavior: with no allowlist set, all current tables
/// show up (well under the 50 default top-N) and both series are
/// emitted with stable, non-zero values that reflect inserts.
#[test]
fn table_metrics_default_top_n_emits_rows_and_bytes_per_table() {
    let addr = pick_free_addr();
    let http = pick_free_addr();
    let mut c = ChildGuard(spawn_server(&addr, &http, &[]));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE alpha (id INT NOT NULL, v INT NOT NULL)",
    );
    exec_ok(
        &mut s,
        "CREATE TABLE beta (id INT NOT NULL, name TEXT NOT NULL)",
    );
    for i in 0..5 {
        exec_ok(
            &mut s,
            &format!("INSERT INTO alpha VALUES ({i}, {})", i * 7),
        );
    }
    for i in 0..3 {
        exec_ok(&mut s, &format!("INSERT INTO beta VALUES ({i}, 'b-{i}')"));
    }

    let body = fetch_metrics(&http);
    assert!(
        body.starts_with("HTTP/1.1 200"),
        "/metrics not 200:\n{body}"
    );
    for needle in [
        "# TYPE spg_table_rows gauge",
        "# TYPE spg_table_bytes gauge",
        "spg_table_rows{table=\"alpha\"} 5",
        "spg_table_rows{table=\"beta\"} 3",
        // bytes is rows × schema-width estimate. alpha = 2×INT = 8
        // bytes/row → 5 rows × 8 = 40. beta = INT + TEXT estimate
        // (4 + 64) = 68 bytes/row → 3 × 68 = 204.
        "spg_table_bytes{table=\"alpha\"} 40",
        "spg_table_bytes{table=\"beta\"} 204",
    ] {
        assert!(
            body.contains(needle),
            "/metrics missing {needle:?}:\n{body}"
        );
    }
}

/// Allowlist mode: only the named tables appear, even if they're
/// smaller. Tables not in the list are dropped.
#[test]
fn table_metrics_allowlist_filters_and_orders() {
    let addr = pick_free_addr();
    let http = pick_free_addr();
    let mut c = ChildGuard(spawn_server(
        &addr,
        &http,
        &[("SPG_METRICS_TABLE_ALLOWLIST", "kept,also_kept".to_string())],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE kept (id INT NOT NULL)");
    exec_ok(&mut s, "CREATE TABLE also_kept (id INT NOT NULL)");
    exec_ok(&mut s, "CREATE TABLE dropped (id INT NOT NULL)");
    for i in 0..2 {
        exec_ok(&mut s, &format!("INSERT INTO kept VALUES ({i})"));
        exec_ok(&mut s, &format!("INSERT INTO also_kept VALUES ({i})"));
        exec_ok(&mut s, &format!("INSERT INTO dropped VALUES ({i})"));
    }

    let body = fetch_metrics(&http);
    assert!(body.contains("spg_table_rows{table=\"kept\"} 2"));
    assert!(body.contains("spg_table_rows{table=\"also_kept\"} 2"));
    assert!(
        !body.contains("spg_table_rows{table=\"dropped\""),
        "dropped table must not appear when allowlist is set:\n{body}"
    );
}

/// Top-N cardinality cap: with many tables and a small N, only the
/// top-N largest by row count are exported.
#[test]
fn table_metrics_topn_caps_cardinality_under_load() {
    let addr = pick_free_addr();
    let http = pick_free_addr();
    let mut c = ChildGuard(spawn_server(
        &addr,
        &http,
        &[("SPG_METRICS_TABLE_TOPN", "3".to_string())],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    for t in 0..6 {
        exec_ok(&mut s, &format!("CREATE TABLE t{t} (id INT NOT NULL)"));
        for i in 0..(10 - t) {
            // t0 has 10 rows, t1 has 9, ..., t5 has 5.
            exec_ok(&mut s, &format!("INSERT INTO t{t} VALUES ({i})"));
        }
    }

    let body = fetch_metrics(&http);
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("spg_table_rows{table="))
        .collect();
    assert_eq!(
        lines.len(),
        3,
        "TOPN=3 should expose exactly 3 spg_table_rows lines, saw {}:\n{body}",
        lines.len()
    );
    // The three biggest by row count are t0, t1, t2.
    for t in 0..3 {
        let needle = format!("spg_table_rows{{table=\"t{t}\"}}");
        assert!(body.contains(&needle), "missing top-N entry {needle:?}");
    }
    for t in 3..6 {
        let needle = format!("spg_table_rows{{table=\"t{t}\"}}");
        assert!(
            !body.contains(&needle),
            "smaller table {needle:?} must not be exported when topn=3"
        );
    }
}
