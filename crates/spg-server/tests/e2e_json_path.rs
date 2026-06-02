#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.14 JSON path operators — -> and ->>.

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

fn select_first_cell(s: &mut TcpStream, sql: &str) -> WireValue {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let dr = read_frame(s);
    let cells = match dr.op {
        Op::DataRow => parse_data_row(&dr).unwrap(),
        Op::DataRowBatch => parse_data_row_batch(&dr)
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
        other => panic!("expected DataRow, got {other:?}"),
    };
    let _ = read_frame(s); // CC
    cells.into_iter().next().unwrap()
}

#[test]
fn arrow_get_text_unwraps_string_value() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE d (body JSON NOT NULL)");
    exec_ok(
        &mut s,
        r#"INSERT INTO d VALUES ('{"name":"alice","age":30}')"#,
    );
    // ->> returns text, no enclosing quotes.
    let v = select_first_cell(&mut s, "SELECT body ->> 'name' FROM d");
    match v {
        WireValue::Text(t) => assert_eq!(t, "alice"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn arrow_get_returns_quoted_json() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE d (body JSON NOT NULL)");
    exec_ok(&mut s, r#"INSERT INTO d VALUES ('{"name":"alice"}')"#);
    // -> returns the JSON sub-value (a string here, wrapped in
    // double quotes when rendered).
    let v = select_first_cell(&mut s, "SELECT body -> 'name' FROM d");
    match v {
        WireValue::Text(t) => assert_eq!(t, "\"alice\""),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn arrow_into_nested_object_returns_subtree() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE d (body JSON NOT NULL)");
    exec_ok(
        &mut s,
        r#"INSERT INTO d VALUES ('{"user":{"name":"bob","age":42}}')"#,
    );
    let v = select_first_cell(&mut s, "SELECT body -> 'user' FROM d");
    match v {
        WireValue::Text(t) => {
            assert!(t.contains("\"name\":\"bob\""));
            assert!(t.contains("\"age\":"));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn arrow_into_array_index_returns_element() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE d (body JSON NOT NULL)");
    exec_ok(&mut s, "INSERT INTO d VALUES ('[10,20,30]')");
    let v = select_first_cell(&mut s, "SELECT body ->> 1 FROM d");
    match v {
        WireValue::Text(t) => assert_eq!(t, "20"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn arrow_missing_key_returns_null() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE d (body JSON NOT NULL)");
    exec_ok(&mut s, r#"INSERT INTO d VALUES ('{"a":1}')"#);
    let v = select_first_cell(&mut s, "SELECT body ->> 'missing' FROM d");
    assert!(matches!(v, WireValue::Null), "got {v:?}");
}

#[test]
fn arrow_works_in_where_clause() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(
        &mut s,
        "CREATE TABLE d (id INT NOT NULL, body JSON NOT NULL)",
    );
    exec_ok(&mut s, r#"INSERT INTO d VALUES (1, '{"status":"open"}')"#);
    exec_ok(&mut s, r#"INSERT INTO d VALUES (2, '{"status":"closed"}')"#);
    exec_ok(&mut s, r#"INSERT INTO d VALUES (3, '{"status":"open"}')"#);
    let v = select_first_cell(
        &mut s,
        "SELECT count(*) FROM d WHERE body ->> 'status' = 'open'",
    );
    match v {
        WireValue::BigInt(n) => assert_eq!(n, 2),
        WireValue::Int(n) => assert_eq!(i64::from(n), 2),
        other => panic!("got {other:?}"),
    }
}
