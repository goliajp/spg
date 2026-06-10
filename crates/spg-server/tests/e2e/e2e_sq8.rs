//! End-to-end roundtrip for `VECTOR(N) USING SQ8` columns.
//!
//! Drives the full spg-server stack (pgwire client → parser →
//! engine → storage → HNSW) and confirms:
//!   * CREATE TABLE with `USING SQ8` lands an SQ8-encoded column
//!     in the catalog;
//!   * INSERT VALUES with raw `[..]` vector literals get quantised
//!     at the engine boundary;
//!   * SELECT projects the cell back dequantised to f32 on the
//!     wire (so pgvector-style clients stay compatible);
//!   * ORDER BY `<->` LIMIT N returns the ground-truth top-N order
//!     with the rerank pass active.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_command_complete, parse_data_row};

use crate::common::{ChildGuard, ServerBuilder, connect_to};

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
fn sq8_create_insert_select_roundtrip_preserves_topk_order() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) USING SQ8 NOT NULL)",
    );
    expect_cc(&mut s);

    // Five rows. Distances from [1, 2, 3, 4] (squared L2):
    //   id=1 (self):  0
    //   id=5 ([1,2,3,5]): 1
    //   id=4 ([2,3,4,5]): 4
    //   id=2 ([4,5,6,7]): 36
    //   id=3 ([6,7,8,9]): 100
    let inserts = [
        (1, "[1.0, 2.0, 3.0, 4.0]"),
        (2, "[4.0, 5.0, 6.0, 7.0]"),
        (3, "[6.0, 7.0, 8.0, 9.0]"),
        (4, "[2.0, 3.0, 4.0, 5.0]"),
        (5, "[1.0, 2.0, 3.0, 5.0]"),
    ];
    for (id, v) in inserts {
        send_query(&mut s, &format!("INSERT INTO emb VALUES ({id}, {v})"));
        expect_cc(&mut s);
    }

    let rows = run_select(
        &mut s,
        "SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3",
    );
    assert_eq!(rows.len(), 3, "kNN should return exactly 3 rows");
    let ids: Vec<WireValue> = rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![WireValue::Int(1), WireValue::Int(5), WireValue::Int(4)],
        "SQ8 ORDER BY <-> must match the f32 ground-truth top-3 order",
    );
}

#[test]
fn sq8_select_dequantises_cell_to_pgvector_wire_shape() {
    // Verify the wire layer hands clients an `[x, y, z, w]` payload
    // (Value::Sq8Vector → WireValue::Vector(Vec<f32>) via dequant).
    // The dequantised values land within `(max-min) / 255 / 2` of
    // the input, which we treat as the recall-budget bound.
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, v VECTOR(4) USING SQ8 NOT NULL)",
    );
    expect_cc(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1, [0.0, 0.25, 0.5, 1.0])");
    expect_cc(&mut s);

    let rows = run_select(&mut s, "SELECT v FROM t");
    assert_eq!(rows.len(), 1);
    let WireValue::Vector(got) = &rows[0][0] else {
        panic!("expected WireValue::Vector, got {:?}", rows[0][0]);
    };
    assert_eq!(got.len(), 4);
    let expected = [0.0_f32, 0.25, 0.5, 1.0];
    // SQ8 envelope: per-byte resolution = (max - min) / 255.
    // Round-trip error stays within half a quantisation step + a
    // tiny float-rounding fudge.
    let tol = (1.0_f32 - 0.0) / 255.0 / 2.0 + 1e-6;
    for (g, e) in got.iter().zip(expected.iter()) {
        let err = (g - e).abs();
        assert!(
            err <= tol,
            "dequantised cell {got:?} vs expected {expected:?}: \
             component diff {err} exceeds tol {tol}"
        );
    }
}
