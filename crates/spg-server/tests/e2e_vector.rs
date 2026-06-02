//! pgvector-style vector search end-to-end:
//! - VECTOR(N) column accepts `[..]` literals on INSERT.
//! - SELECT ORDER BY col `<->` literal LIMIT N returns the N nearest rows
//!   in ascending L2-distance order.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_command_complete, parse_data_row};

mod common;
use common::{ChildGuard, ServerBuilder, connect_to};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn expect_cc(s: &mut TcpStream) {
    let f = read_frame(s);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn run_select(s: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send_query(s, sql);
    let rd = read_frame(s);
    if rd.op != Op::RowDescription {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("expected RD, got {:?}: {msg}", rd.op);
    }
    let mut rows = Vec::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => rows.extend(spg_wire::parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return rows,
            Op::ErrorResponse => {
                let msg = spg_wire::parse_error_response(&f).unwrap();
                panic!("server error mid-row-stream: {msg}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[test]
fn k_nearest_l2_distance_search_returns_top_k_in_order() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Two-column table — id + 3-d vector.
    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(3) NOT NULL)",
    );
    expect_cc(&mut s);

    // Seed five rows. The L2 distance from [1, 2, 3] is, in order:
    //   id=1: 0           (self)
    //   id=5: ~1.0        ([1, 2, 4])
    //   id=4: ~1.732      ([2, 3, 4])
    //   id=2: ~5.196      ([4, 5, 6])
    //   id=3: ~8.660      ([6, 7, 8])
    let inserts = [
        (1, "[1.0, 2.0, 3.0]"),
        (2, "[4.0, 5.0, 6.0]"),
        (3, "[6.0, 7.0, 8.0]"),
        (4, "[2.0, 3.0, 4.0]"),
        (5, "[1.0, 2.0, 4.0]"),
    ];
    for (id, v) in inserts {
        send_query(&mut s, &format!("INSERT INTO emb VALUES ({id}, {v})"));
        expect_cc(&mut s);
    }

    let rows = run_select(
        &mut s,
        "SELECT * FROM emb ORDER BY v <-> [1.0, 2.0, 3.0] LIMIT 3",
    );
    assert_eq!(rows.len(), 3, "kNN should return exactly 3 rows");

    // Closest three by id, in distance order: 1, 5, 4.
    let ids: Vec<WireValue> = rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![WireValue::Int(1), WireValue::Int(5), WireValue::Int(4)],
        "rows must come back in ascending L2-distance order"
    );
}

#[test]
fn order_by_distance_without_limit_returns_all_rows_sorted() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(2) NOT NULL)",
    );
    expect_cc(&mut s);
    for (id, v) in [(10, "[0.0, 0.0]"), (20, "[3.0, 4.0]"), (30, "[1.0, 1.0]")] {
        send_query(&mut s, &format!("INSERT INTO emb VALUES ({id}, {v})"));
        expect_cc(&mut s);
    }

    let rows = run_select(&mut s, "SELECT id FROM emb ORDER BY v <-> [0.0, 0.0]");
    let ids: Vec<WireValue> = rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![WireValue::Int(10), WireValue::Int(30), WireValue::Int(20)]
    );
}

#[test]
fn vector_dim_mismatch_at_insert_errors() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE TABLE emb (v VECTOR(3) NOT NULL)");
    expect_cc(&mut s);
    send_query(&mut s, "INSERT INTO emb VALUES ([1.0, 2.0])"); // 2-d into 3-d column
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = spg_wire::parse_error_response(&f).unwrap();
    assert!(
        msg.to_ascii_lowercase().contains("type") || msg.to_ascii_lowercase().contains("mismatch"),
        "expected type-mismatch error, got {msg:?}"
    );
}
