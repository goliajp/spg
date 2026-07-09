//! v4.9 JSON column type — round-trip through the native wire.
//! No path operators yet; the column just stores and returns the
//! verbatim JSON string.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row};

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
    assert_eq!(
        f.op,
        Op::CommandComplete,
        "expected CC for {sql:?}, got {:?}",
        f.op
    );
}

#[test]
fn json_column_round_trips_arbitrary_payload() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE doc (id INT NOT NULL, body JSON NOT NULL)",
    );
    let payload = r#"{"name":"alice","scores":[1,2,3],"nested":{"k":true}}"#;
    exec_ok(&mut s, &format!("INSERT INTO doc VALUES (1, '{payload}')"));

    send(&mut s, &build_query("SELECT body FROM doc"));
    let rd = read_frame(&mut s);
    assert_eq!(rd.op, Op::RowDescription);
    let dr = read_frame(&mut s);
    assert_eq!(dr.op, Op::DataRow);
    let cells = parse_data_row(&dr).unwrap();
    assert_eq!(cells.len(), 1);
    match &cells[0] {
        WireValue::Text(s) => assert_eq!(s, payload),
        other => panic!("expected Text wire value for JSON, got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}

#[test]
fn jsonb_alias_accepted() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // JSONB should map to the same JSON column type; PG drivers use
    // it interchangeably.
    exec_ok(&mut s, "CREATE TABLE d (b JSONB NOT NULL)");
    exec_ok(&mut s, "INSERT INTO d VALUES ('{\"a\":1}')");
    send(&mut s, &build_query("SELECT b FROM d"));
    let _rd = read_frame(&mut s);
    let dr = read_frame(&mut s);
    let cells = parse_data_row(&dr).unwrap();
    match &cells[0] {
        // v7.38 — a JSONB value renders canonically (`{"a": 1}`, space after the
        // colon), matching PG; the pre-canonicalization spelling `{"a":1}` is
        // what a JSON (non-b) column would keep.
        WireValue::Text(s) => assert!(s.contains("\"a\": 1"), "got {s}"),
        other => panic!("got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}

#[test]
fn update_json_column() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE doc (id INT NOT NULL, body JSON NOT NULL)",
    );
    exec_ok(&mut s, "INSERT INTO doc VALUES (1, '{\"v\":1}')");
    exec_ok(&mut s, "UPDATE doc SET body = '{\"v\":2}' WHERE id = 1");

    send(&mut s, &build_query("SELECT body FROM doc"));
    let _rd = read_frame(&mut s);
    let dr = read_frame(&mut s);
    let cells = parse_data_row(&dr).unwrap();
    match &cells[0] {
        WireValue::Text(s) => assert_eq!(s, "{\"v\":2}"),
        other => panic!("got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}
