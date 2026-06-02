#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.12 window functions — ROW_NUMBER / RANK / DENSE_RANK and the
//! partition-aware aggregates.

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

fn seed(s: &mut TcpStream) {
    exec_ok(
        s,
        "CREATE TABLE sales (region TEXT NOT NULL, amt INT NOT NULL)",
    );
    for (r, a) in [
        ("east", 10),
        ("east", 30),
        ("east", 20),
        ("west", 5),
        ("west", 15),
    ] {
        exec_ok(s, &format!("INSERT INTO sales VALUES ('{r}', {a})"));
    }
}

fn as_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn row_number_over_partition_and_order() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    let rows = select_rows(
        &mut s,
        "SELECT region, amt, ROW_NUMBER() OVER (PARTITION BY region ORDER BY amt) FROM sales",
    );
    // Expect 5 rows. Per-region rank in amt order:
    //   east: 10→1, 20→2, 30→3
    //   west: 5→1, 15→2
    let mut got: Vec<(String, i64, i64)> = rows
        .iter()
        .map(|r| {
            let region = match &r[0] {
                WireValue::Text(t) => t.clone(),
                other => panic!("got {other:?}"),
            };
            (region, as_i64(&r[1]), as_i64(&r[2]))
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("east".into(), 10, 1),
            ("east".into(), 20, 2),
            ("east".into(), 30, 3),
            ("west".into(), 5, 1),
            ("west".into(), 15, 2),
        ]
    );
}

#[test]
fn sum_over_partition_running_total() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    // ORDER BY present → running total within partition.
    let rows = select_rows(
        &mut s,
        "SELECT region, amt, SUM(amt) OVER (PARTITION BY region ORDER BY amt) FROM sales",
    );
    let mut got: Vec<(String, i64, f64)> = rows
        .iter()
        .map(|r| {
            let region = match &r[0] {
                WireValue::Text(t) => t.clone(),
                other => panic!("got {other:?}"),
            };
            let f = match &r[2] {
                WireValue::Float(f) => *f,
                WireValue::Text(t) => t.parse().unwrap(),
                other => panic!("got {other:?}"),
            };
            (region, as_i64(&r[1]), f)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    // east running sum: 10, 30 (10+20), 60 (10+20+30)
    // west running sum: 5, 20 (5+15)
    assert_eq!(got[0], ("east".into(), 10, 10.0));
    assert_eq!(got[1], ("east".into(), 20, 30.0));
    assert_eq!(got[2], ("east".into(), 30, 60.0));
    assert_eq!(got[3], ("west".into(), 5, 5.0));
    assert_eq!(got[4], ("west".into(), 15, 20.0));
}

#[test]
fn count_over_no_partition() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed(&mut s);

    // No PARTITION BY → one partition (everything). No ORDER BY →
    // whole-partition aggregate. Every row should see 5.
    let rows = select_rows(&mut s, "SELECT amt, COUNT(*) OVER () FROM sales");
    assert_eq!(rows.len(), 5);
    for r in &rows {
        assert_eq!(as_i64(&r[1]), 5);
    }
}
