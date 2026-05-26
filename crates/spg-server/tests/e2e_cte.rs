#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.11 WITH / CTE — uncorrelated, non-recursive.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

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
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

fn select_first_col(s: &mut TcpStream, sql: &str) -> Vec<WireValue> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
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
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn single_cte_select_from_it() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL, val INT NOT NULL)");
    for (i, v) in [(1, 10), (2, 20), (3, 30)] {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i}, {v})"));
    }

    let rows = select_first_col(
        &mut s,
        "WITH big AS (SELECT id FROM t WHERE val >= 20) SELECT id FROM big",
    );
    let ids: Vec<i32> = rows
        .into_iter()
        .map(|v| match v {
            WireValue::Int(n) => n,
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn multiple_ctes_chain_through_join_or_filter() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE orders (id INT NOT NULL, status TEXT NOT NULL)",
    );
    exec_ok(&mut s, "INSERT INTO orders VALUES (1, 'paid')");
    exec_ok(&mut s, "INSERT INTO orders VALUES (2, 'pending')");
    exec_ok(&mut s, "INSERT INTO orders VALUES (3, 'paid')");

    // Two CTEs in the same WITH clause — both materialise off the
    // original orders table (siblings don't see each other).
    let rows = select_first_col(
        &mut s,
        "WITH paid_ids AS (SELECT id FROM orders WHERE status = 'paid'), \
              pending_ids AS (SELECT id FROM orders WHERE status = 'pending') \
         SELECT id FROM paid_ids",
    );
    let ids: Vec<i32> = rows
        .into_iter()
        .map(|v| match v {
            WireValue::Int(n) => n,
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn cte_with_aggregate() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE sales (id INT NOT NULL, amt INT NOT NULL)",
    );
    for v in [10, 20, 30, 40] {
        exec_ok(&mut s, &format!("INSERT INTO sales VALUES (0, {v})"));
    }

    // CTE produces an aggregate, body filters on it.
    let rows = select_first_col(
        &mut s,
        "WITH totals AS (SELECT sum(amt) AS total FROM sales) SELECT total FROM totals",
    );
    assert_eq!(rows.len(), 1);
    match &rows[0] {
        WireValue::BigInt(n) => assert_eq!(*n, 100),
        WireValue::Int(n) => assert_eq!(*n, 100),
        other => panic!("got {other:?}"),
    }
}
