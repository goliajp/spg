//! End-to-end SPG: spawn the daemon binary, drive it with raw wire frames,
//! verify the full chain CREATE → INSERT → SELECT works. Lives in the
//! `spg-server` crate because that's the only one whose integration tests
//! have `CARGO_BIN_EXE_spg-server` injected by cargo.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::{ChildGuard, ServerBuilder, connect_to};

use spg_wire::{
    Frame, Op, WireType, WireValue, build_query, encode, parse_command_complete, parse_data_row,
    parse_error_response, parse_row_description,
};

const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

/// Read exactly one frame off the wire. Uses `read_exact` so we never drop
/// already-arrived bytes between consecutive frames.
fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read frame header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read frame payload");
    }
    Frame { op, payload }
}

#[test]
fn create_insert_select_full_cycle() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut stream = connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );
    assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);

    for sql in [
        "INSERT INTO t VALUES (1, 'alice')",
        "INSERT INTO t VALUES (2, 'bob')",
    ] {
        send_query(&mut stream, sql);
        let f = read_frame(&mut stream);
        assert_eq!(f.op, Op::CommandComplete);
        assert_eq!(parse_command_complete(&f).unwrap(), 1);
    }

    send_query(&mut stream, "SELECT * FROM t");
    let rd = read_frame(&mut stream);
    assert_eq!(rd.op, Op::RowDescription);
    let cols = parse_row_description(&rd).unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].ty, WireType::Int);
    assert_eq!(cols[1].name, "name");
    assert_eq!(cols[1].ty, WireType::Text);

    let mut rows = Vec::new();
    loop {
        let f = read_frame(&mut stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => rows.extend(spg_wire::parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], WireValue::Int(1));
    assert_eq!(rows[0][1], WireValue::Text("alice".into()));
    assert_eq!(rows[1][0], WireValue::Int(2));
    assert_eq!(rows[1][1], WireValue::Text("bob".into()));
}

#[test]
fn select_with_where_via_wire() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut stream = connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut stream, "CREATE TABLE nums (x INT NOT NULL)");
    assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);
    for sql in [
        "INSERT INTO nums VALUES (1)",
        "INSERT INTO nums VALUES (2)",
        "INSERT INTO nums VALUES (3)",
    ] {
        send_query(&mut stream, sql);
        assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);
    }
    send_query(&mut stream, "SELECT * FROM nums WHERE x > 1");
    assert_eq!(read_frame(&mut stream).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(&mut stream);
        match f.op {
            Op::DataRow => count += 1,
            Op::DataRowBatch => count += spg_wire::parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(count, 2);
}

#[test]
fn syntax_error_returns_error_response() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut stream = connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut stream, "DROP TABLE foo"); // not in parser scope yet
    let f = read_frame(&mut stream);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.to_ascii_lowercase().contains("parse") || msg.contains("expected"),
        "error message should mention parsing — got {msg:?}"
    );
}

#[test]
fn second_connection_sees_first_connection_writes() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut a = connect_to(&addrs.native);
    a.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut a, "CREATE TABLE shared (v INT)");
    assert_eq!(read_frame(&mut a).op, Op::CommandComplete);
    send_query(&mut a, "INSERT INTO shared VALUES (42)");
    assert_eq!(read_frame(&mut a).op, Op::CommandComplete);

    let mut b = TcpStream::connect(&addrs.native).unwrap();
    b.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut b, "SELECT * FROM shared");
    assert_eq!(read_frame(&mut b).op, Op::RowDescription);
    let dr = read_frame(&mut b);
    assert_eq!(dr.op, Op::DataRow);
    let values = parse_data_row(&dr).unwrap();
    assert_eq!(values[0], WireValue::Int(42));
    assert_eq!(read_frame(&mut b).op, Op::CommandComplete);
}
