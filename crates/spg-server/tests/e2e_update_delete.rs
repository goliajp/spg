#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.4 UPDATE / DELETE end-to-end:
//! - basic UPDATE SET col=expr [WHERE]
//! - DELETE FROM with WHERE filter
//! - indexed-column UPDATE rebuilds the index (subsequent indexed
//!   SELECT sees the new value)
//! - DELETE without WHERE is a full-table truncate

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

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
    assert_eq!(
        f.op,
        Op::CommandComplete,
        "expected CC for {sql:?}, got {:?}",
        f.op
    );
}

/// Run a SELECT and collect every row's first cell as a string.
/// Drains DataRow + DataRowBatch indifferently.
fn select_first_col(s: &mut TcpStream, sql: &str) -> Vec<spg_wire::WireValue> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription, "expected RD, got {:?}", rd.op);
    let mut out = Vec::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => out.push(parse_data_row(&f).unwrap().into_iter().next().unwrap()),
            Op::DataRowBatch => {
                for row in parse_data_row_batch(&f).unwrap() {
                    out.push(row.into_iter().next().unwrap());
                }
            }
            Op::CommandComplete => return out,
            other => panic!("unexpected mid-stream: {other:?}"),
        }
    }
}

#[test]
fn update_set_where_changes_only_matched_rows() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)",
    );
    for i in 1..=4 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, 'old-{i}')"));
    }
    exec_ok(&mut s, "UPDATE t SET label = 'new' WHERE id = 2");

    let labels = select_first_col(&mut s, "SELECT label FROM t");
    let texts: Vec<String> = labels
        .into_iter()
        .map(|v| match v {
            spg_wire::WireValue::Text(s) => s,
            other => panic!("unexpected wire value: {other:?}"),
        })
        .collect();
    assert_eq!(texts, vec!["old-1", "new", "old-3", "old-4"]);
}

#[test]
fn delete_with_where_drops_matched_rows() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 1..=5 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    exec_ok(&mut s, "DELETE FROM t WHERE id = 3");

    let ids = select_first_col(&mut s, "SELECT id FROM t");
    let int_ids: Vec<i32> = ids
        .into_iter()
        .map(|v| match v {
            spg_wire::WireValue::Int(n) => n,
            other => panic!("unexpected wire value: {other:?}"),
        })
        .collect();
    assert_eq!(int_ids, vec![1, 2, 4, 5]);
}

#[test]
fn delete_without_where_truncates_table() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 1..=3 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    exec_ok(&mut s, "DELETE FROM t");
    let ids = select_first_col(&mut s, "SELECT id FROM t");
    assert!(ids.is_empty(), "DELETE FROM t should empty the table");
}

#[test]
fn update_on_indexed_column_keeps_index_consistent() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)",
    );
    exec_ok(&mut s, "CREATE INDEX t_id_idx ON t (id)");
    for i in 1..=4 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, 'r-{i}')"));
    }
    // Renumber row id=2 to id=42 — without index rebuild, indexed
    // SELECT WHERE id=42 would miss it (and WHERE id=2 would still
    // find the now-stale row).
    exec_ok(&mut s, "UPDATE t SET id = 42 WHERE id = 2");

    let labels = select_first_col(&mut s, "SELECT label FROM t WHERE id = 42");
    assert_eq!(labels.len(), 1, "expected one row after re-keying");
    assert!(matches!(&labels[0], spg_wire::WireValue::Text(s) if s == "r-2"));

    let stale = select_first_col(&mut s, "SELECT label FROM t WHERE id = 2");
    assert!(stale.is_empty(), "old id should no longer be indexed");
}
