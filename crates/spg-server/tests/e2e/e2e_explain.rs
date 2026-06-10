//! v4.26 EXPLAIN / EXPLAIN ANALYZE — single-column QUERY PLAN
//! text table describing the rewritten plan tree.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use crate::common::{ChildGuard, ServerBuilder, connect_to};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

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
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1, 10)");

    let lines = explain_lines(&mut s, "EXPLAIN SELECT * FROM t");
    let blob = lines.join("\n");
    assert!(blob.contains("TableScan"), "missing TableScan: {blob}");
    assert!(blob.contains("From: t"), "missing From: t: {blob}");
    assert!(
        blob.contains("full scan"),
        "expected full scan note: {blob}"
    );
}

#[test]
fn explain_picks_up_index_seek() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
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
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
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
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 1..=7 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }

    let lines = explain_lines(&mut s, "EXPLAIN ANALYZE SELECT * FROM t WHERE id > 4");
    let blob = lines.join("\n");
    // v6.2.4 changed the EXPLAIN ANALYZE annotation shape: per-op
    // `(rows=N)` inline on each operator line plus a `Total:` line
    // at the bottom carrying total rows + elapsed micros. Drop the
    // legacy "Actual:" probe; assert on the new shape instead.
    assert!(
        blob.contains("Total:") && blob.contains("rows=3"),
        "expected Total: + rows=3 on a `id > 4` filter over 7 rows; got: {blob}"
    );
}

#[test]
fn explain_window_function_labels_window_op() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE w (n INT NOT NULL, v INT NOT NULL)");
    exec_ok(&mut s, "INSERT INTO w VALUES (1, 10)");

    let lines = explain_lines(
        &mut s,
        "EXPLAIN SELECT n, ROW_NUMBER() OVER (ORDER BY n) FROM w",
    );
    let blob = lines.join("\n");
    assert!(blob.contains("WindowAgg"), "missing WindowAgg: {blob}");
}
