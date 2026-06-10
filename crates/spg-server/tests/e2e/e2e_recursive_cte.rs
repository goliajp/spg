//! v4.22 WITH RECURSIVE — counter, descendant tree, dedup, runaway guard.

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

#[test]
fn counter_one_to_ten() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let rows = select_rows(
        &mut s,
        "WITH RECURSIVE t(n) AS (\
            SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10\
         ) SELECT * FROM t",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    assert_eq!(got, (1..=10).collect::<Vec<_>>());
}

#[test]
fn anchor_referencing_self_is_rejected() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(
        &mut s,
        &build_query(
            "WITH RECURSIVE t AS (SELECT * FROM t UNION ALL SELECT * FROM t) SELECT * FROM t",
        ),
    );
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
}

#[test]
fn descendants_over_graph_table() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE edges (parent INT NOT NULL, child INT NOT NULL)",
    );
    // 1 → 2 → 4; 1 → 3 → 5; 3 → 6.
    for (p, c) in [(1, 2), (1, 3), (2, 4), (3, 5), (3, 6)] {
        exec_ok(&mut s, &format!("INSERT INTO edges VALUES ({p}, {c})"));
    }

    let rows = select_rows(
        &mut s,
        "WITH RECURSIVE d(node) AS (\
            SELECT child FROM edges WHERE parent = 1 \
            UNION ALL \
            SELECT e.child FROM edges e JOIN d ON e.parent = d.node\
         ) SELECT * FROM d",
    );
    let mut got: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    got.sort_unstable();
    assert_eq!(got, vec![2, 3, 4, 5, 6]);
}

#[test]
fn union_distinct_dedups() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Without UNION dedup, this would oscillate forever between
    // (1) and (1) — but UNION (DISTINCT) catches the repeat.
    let rows = select_rows(
        &mut s,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT 1 FROM t) SELECT * FROM t",
    );
    assert_eq!(rows.len(), 1, "expected dedup → 1 row, got {}", rows.len());
}

#[test]
fn iteration_cap_rejects_runaway() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // UNION ALL with no termination condition — should hit the
    // row-count cap and surface a clear error rather than hang.
    send(
        &mut s,
        &build_query(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT * FROM t",
        ),
    );
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse, "expected runaway-recursion error");
}
