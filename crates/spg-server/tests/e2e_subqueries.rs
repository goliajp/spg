#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.10 subqueries — scalar / EXISTS / IN.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

mod common;
use common::{ChildGuard, ServerBuilder, connect_to};

const READ_TIMEOUT: Duration = Duration::from_secs(3);

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
    assert_eq!(rd.op, Op::RowDescription);
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

fn seed(s: &mut TcpStream) {
    exec_ok(s, "CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)");
    exec_ok(s, "CREATE TABLE allowed (val INT NOT NULL)");
    for i in 1..=5 {
        exec_ok(s, &format!("INSERT INTO t VALUES ({i}, 'r-{i}')"));
    }
    exec_ok(s, "INSERT INTO allowed VALUES (2)");
    exec_ok(s, "INSERT INTO allowed VALUES (4)");
}

#[test]
fn scalar_subquery_returns_count() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    // (SELECT count(*) FROM allowed) should evaluate to 2.
    let rows = select_first_col(&mut s, "SELECT (SELECT count(*) FROM allowed)");
    assert_eq!(rows.len(), 1);
    match &rows[0] {
        WireValue::BigInt(n) => assert_eq!(*n, 2),
        WireValue::Int(n) => assert_eq!(*n, 2),
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[test]
fn exists_subquery_filters_rows() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    // EXISTS evaluates to constant true (there are rows in allowed),
    // so the predicate keeps every row of t.
    let rows = select_first_col(
        &mut s,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM allowed)",
    );
    assert_eq!(rows.len(), 5);

    // EXISTS over an empty result → constant false → zero rows.
    exec_ok(&mut s, "DELETE FROM allowed");
    let rows = select_first_col(
        &mut s,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM allowed)",
    );
    assert!(rows.is_empty(), "expected zero rows after EXISTS-empty");
}

#[test]
fn in_subquery_filters_to_intersection() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    let rows = select_first_col(
        &mut s,
        "SELECT id FROM t WHERE id IN (SELECT val FROM allowed)",
    );
    let ids: Vec<i32> = rows
        .into_iter()
        .map(|v| match v {
            WireValue::Int(n) => n,
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![2, 4]);
}

#[test]
fn not_in_subquery_returns_complement() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    let rows = select_first_col(
        &mut s,
        "SELECT id FROM t WHERE id NOT IN (SELECT val FROM allowed)",
    );
    let ids: Vec<i32> = rows
        .into_iter()
        .map(|v| match v {
            WireValue::Int(n) => n,
            other => panic!("got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 3, 5]);
}
