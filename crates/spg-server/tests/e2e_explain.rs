#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.26 EXPLAIN / EXPLAIN ANALYZE — single-column QUERY PLAN
//! text table describing the rewritten plan tree.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn spawn_server(addr: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .spawn()
        .unwrap()
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
    let f = read_frame(s);
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

fn explain_lines(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut out = Vec::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => {
                let row = parse_data_row(&f).unwrap();
                if let Some(WireValue::Text(t)) = row.first() {
                    out.push(t.clone());
                }
            }
            Op::DataRowBatch => {
                for row in parse_data_row_batch(&f).unwrap() {
                    if let Some(WireValue::Text(t)) = row.first() {
                        out.push(t.clone());
                    }
                }
            }
            Op::CommandComplete => return out,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn explain_simple_table_scan_reports_full_scan() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1, 10)");

    let lines = explain_lines(&mut s, "EXPLAIN SELECT * FROM t");
    let blob = lines.join("\n");
    assert!(blob.contains("TableScan"), "missing TableScan: {blob}");
    assert!(blob.contains("From: t"), "missing From: t: {blob}");
    assert!(blob.contains("full scan"), "expected full scan note: {blob}");
}

#[test]
fn explain_picks_up_index_seek() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    exec_ok(&mut s, "CREATE INDEX t_id_idx ON t (id)");
    for i in 1..=10 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, {})", i * 10));
    }

    let lines = explain_lines(&mut s, "EXPLAIN SELECT v FROM t WHERE id = 5");
    let blob = lines.join("\n");
    assert!(
        blob.contains("index seek"),
        "expected index seek note: {blob}"
    );
}

#[test]
fn explain_shows_aggregate_and_grouping() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE sales (region TEXT NOT NULL, amt INT NOT NULL)",
    );
    exec_ok(&mut s, "INSERT INTO sales VALUES ('east', 10)");

    let lines = explain_lines(
        &mut s,
        "EXPLAIN SELECT region, sum(amt) FROM sales GROUP BY region HAVING sum(amt) > 5",
    );
    let blob = lines.join("\n");
    assert!(blob.contains("Aggregate"), "missing Aggregate: {blob}");
    assert!(blob.contains("GroupBy"), "missing GroupBy: {blob}");
    assert!(blob.contains("Having"), "missing Having: {blob}");
}

#[test]
fn explain_analyze_attaches_actual_rows() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 1..=7 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }

    let lines = explain_lines(&mut s, "EXPLAIN ANALYZE SELECT * FROM t WHERE id > 4");
    let blob = lines.join("\n");
    assert!(blob.contains("Actual:"), "missing Actual: {blob}");
    assert!(
        blob.contains("rows=3"),
        "expected rows=3 (id > 4 ⇒ 5/6/7), got: {blob}"
    );
}

#[test]
fn explain_window_function_labels_window_op() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE w (n INT NOT NULL, v INT NOT NULL)",
    );
    exec_ok(&mut s, "INSERT INTO w VALUES (1, 10)");

    let lines = explain_lines(
        &mut s,
        "EXPLAIN SELECT n, ROW_NUMBER() OVER (ORDER BY n) FROM w",
    );
    let blob = lines.join("\n");
    assert!(blob.contains("WindowAgg"), "missing WindowAgg: {blob}");
}
