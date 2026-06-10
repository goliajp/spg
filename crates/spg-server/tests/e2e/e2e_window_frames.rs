//! v4.20 explicit window frames — ROWS / RANGE BETWEEN ... AND ...

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use crate::common::{ChildGuard, ServerBuilder, connect_to};

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

fn as_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn as_f64(v: &WireValue) -> f64 {
    match v {
        WireValue::Float(f) => *f,
        WireValue::Int(n) => f64::from(*n),
        #[allow(clippy::cast_precision_loss)]
        WireValue::BigInt(n) => *n as f64,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected numeric, got {other:?}"),
    }
}

fn seed_series(s: &mut TcpStream) {
    exec_ok(s, "CREATE TABLE ts (n INT NOT NULL, v INT NOT NULL)");
    for (n, v) in [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
        exec_ok(s, &format!("INSERT INTO ts VALUES ({n}, {v})"));
    }
}

#[test]
fn rows_between_unbounded_preceding_and_current_row() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    // Identical to the default ordered frame.
    let rows = select_rows(
        &mut s,
        "SELECT n, SUM(v) OVER (ORDER BY n ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM ts",
    );
    let mut got: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_f64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    assert_eq!(
        got,
        vec![(1, 10.0), (2, 30.0), (3, 60.0), (4, 100.0), (5, 150.0)]
    );
}

#[test]
fn rows_between_one_preceding_and_one_following() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    // 3-row centered window. Edge rows truncate naturally.
    let rows = select_rows(
        &mut s,
        "SELECT n, SUM(v) OVER (ORDER BY n ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM ts",
    );
    let mut got: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_f64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    // n=1: [10,20]=30; n=2: [10,20,30]=60; n=3: [20,30,40]=90;
    // n=4: [30,40,50]=120; n=5: [40,50]=90.
    assert_eq!(
        got,
        vec![(1, 30.0), (2, 60.0), (3, 90.0), (4, 120.0), (5, 90.0)]
    );
}

#[test]
fn rows_between_unbounded_preceding_and_unbounded_following() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    let rows = select_rows(
        &mut s,
        "SELECT n, SUM(v) OVER (ORDER BY n ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM ts",
    );
    assert_eq!(rows.len(), 5);
    for r in &rows {
        assert_eq!(as_f64(&r[1]), 150.0);
    }
}

#[test]
fn rows_shorthand_n_preceding() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    // `ROWS 2 PRECEDING` normalises to `BETWEEN 2 PRECEDING AND CURRENT ROW`.
    let rows = select_rows(
        &mut s,
        "SELECT n, AVG(v) OVER (ORDER BY n ROWS 2 PRECEDING) FROM ts",
    );
    let mut got: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_f64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    // n=1: avg(10)=10; n=2: avg(10,20)=15; n=3: avg(10,20,30)=20;
    // n=4: avg(20,30,40)=30; n=5: avg(30,40,50)=40.
    assert_eq!(
        got,
        vec![(1, 10.0), (2, 15.0), (3, 20.0), (4, 30.0), (5, 40.0)]
    );
}

#[test]
fn range_peer_semantics_with_ties() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(
        &mut s,
        "CREATE TABLE peers (k INT NOT NULL, v INT NOT NULL)",
    );
    // ORDER BY key has ties at k=2 and k=3.
    for (k, v) in [(1, 10), (2, 20), (2, 25), (3, 30), (3, 35)] {
        exec_ok(&mut s, &format!("INSERT INTO peers VALUES ({k}, {v})"));
    }

    // RANGE default (UNBOUNDED PRECEDING AND CURRENT ROW) treats
    // tied rows on `k` as a single peer group — both k=2 rows see
    // the same running sum, and both k=3 rows see the same one too.
    let rows = select_rows(
        &mut s,
        "SELECT k, v, SUM(v) OVER (ORDER BY k RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM peers",
    );
    let mut got: Vec<(i64, i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64(&r[1]), as_f64(&r[2])))
        .collect();
    got.sort_by_key(|(k, v, _)| (*k, *v));
    // k=1: 10
    // k=2 peers: 10+20+25=55 for both
    // k=3 peers: 55+30+35=120 for both
    assert_eq!(got[0], (1, 10, 10.0));
    assert_eq!(got[1], (2, 20, 55.0));
    assert_eq!(got[2], (2, 25, 55.0));
    assert_eq!(got[3], (3, 30, 120.0));
    assert_eq!(got[4], (3, 35, 120.0));
}

#[test]
fn range_with_offset_is_rejected() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    send(
        &mut s,
        &build_query(
            "SELECT n, SUM(v) OVER (ORDER BY n RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM ts",
        ),
    );
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse, "expected error for RANGE offset");
    let msg = spg_wire::parse_error_response(&f).unwrap_or("");
    assert!(
        msg.contains("RANGE") || msg.contains("offset"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn count_star_over_sliding_window() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_series(&mut s);

    let rows = select_rows(
        &mut s,
        "SELECT n, COUNT(*) OVER (ORDER BY n ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM ts",
    );
    let mut got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    // n=1: 2 (no preceding); n=2,3,4: 3; n=5: 2 (no following).
    assert_eq!(got, vec![(1, 2), (2, 3), (3, 3), (4, 3), (5, 2)]);
}
