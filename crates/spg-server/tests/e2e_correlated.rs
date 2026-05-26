#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.23 correlated subqueries — EXISTS / NOT EXISTS / scalar / IN
//! with outer-row references in the WHERE clause.

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

fn select_rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
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
            Op::DataRow => out.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => out.extend(parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return out,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn as_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn seed_orders_customers(s: &mut TcpStream) {
    exec_ok(
        s,
        "CREATE TABLE customers (id INT NOT NULL, name TEXT NOT NULL)",
    );
    for (id, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
        exec_ok(s, &format!("INSERT INTO customers VALUES ({id}, '{name}')"));
    }
    exec_ok(
        s,
        "CREATE TABLE orders (id INT NOT NULL, customer_id INT NOT NULL, total INT NOT NULL)",
    );
    // alice has 2 orders; bob has 1; carol has none.
    for (id, cid, total) in [(101, 1, 50), (102, 1, 30), (103, 2, 100)] {
        exec_ok(
            s,
            &format!("INSERT INTO orders VALUES ({id}, {cid}, {total})"),
        );
    }
}

#[test]
fn correlated_exists_in_where() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_orders_customers(&mut s);

    // Customers with at least one order.
    let rows = select_rows(
        &mut s,
        "SELECT id FROM customers c WHERE EXISTS \
         (SELECT 1 FROM orders WHERE customer_id = c.id)",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
}

#[test]
fn correlated_not_exists_in_where() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_orders_customers(&mut s);

    // Customers with no orders.
    let rows = select_rows(
        &mut s,
        "SELECT id FROM customers c WHERE NOT EXISTS \
         (SELECT 1 FROM orders WHERE customer_id = c.id)",
    );
    let got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(got, vec![3]);
}

#[test]
fn correlated_scalar_subquery_in_where() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_orders_customers(&mut s);

    // Customers whose first-order total > 40 — using a scalar
    // correlated subquery to compute per-customer max total.
    let rows = select_rows(
        &mut s,
        "SELECT id FROM customers c WHERE \
         (SELECT max(total) FROM orders WHERE customer_id = c.id) > 40",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    // alice max=50, bob max=100, carol max=NULL.
    assert_eq!(got, vec![1, 2]);
}

#[test]
fn correlated_in_subquery() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_orders_customers(&mut s);

    // Customers whose id is in the set of customer_ids that have a
    // total > 40 in their orders. (Equivalent to EXISTS here, but
    // exercises the IN-correlation code path.)
    let rows = select_rows(
        &mut s,
        "SELECT id FROM customers c WHERE c.id IN \
         (SELECT customer_id FROM orders WHERE total > 40 AND customer_id = c.id)",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
}

#[test]
fn uncorrelated_subquery_still_optimised_once() {
    // Sanity check: an uncorrelated subquery should still go through
    // the fast pre-eval path, not per-row. Behavior-only assertion —
    // both paths should produce the same result, but the bug fix in
    // is_correlation_error must not over-fall-through.
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_orders_customers(&mut s);

    let rows = select_rows(
        &mut s,
        "SELECT id FROM customers WHERE id IN (SELECT customer_id FROM orders)",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
}
