//! End-to-end roundtrip for `VECTOR(N) USING HALF` columns.
//!
//! Mirrors `e2e_sq8.rs` against the v6.0.3 halfvec path. Drives
//! the full spg-server stack (pgwire → parser → engine → storage
//! → HNSW) and confirms:
//!   * CREATE TABLE with `USING HALF` lands an `F16`-encoded
//!     column in the catalog;
//!   * INSERT VALUES with raw `[..]` vector literals get
//!     converted to half-precision at the engine boundary;
//!   * SELECT projects the cell back dequantised to f32 on the
//!     wire — bit-exact for f16-representable values;
//!   * ORDER BY `<->` LIMIT N returns the f32 ground-truth top-N
//!     order (no rerank pass needed because dequant is exact).

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
fn half_create_insert_select_roundtrip_preserves_topk_order() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) USING HALF NOT NULL)",
    );
    expect_cc(&mut s);

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
    assert_eq!(rows.len(), 3);
    let ids: Vec<WireValue> = rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![WireValue::Int(1), WireValue::Int(5), WireValue::Int(4)],
        "HALF ORDER BY <-> must match f32 top-3 order (dequant is bit-exact)",
    );
}

#[test]
fn half_select_dequantises_cell_to_pgvector_wire_shape() {
    // The pgvector wire shape stays `[x, y, z, w]` f32 — halfvec
    // cells dequant bit-exactly for grid-aligned values, so this
    // test asserts byte-equality on the canonical fixtures.
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, v VECTOR(4) USING HALF NOT NULL)",
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
    // 0.0 / 0.25 / 0.5 / 1.0 all land exactly on the f16 grid.
    let expected = [0.0_f32, 0.25, 0.5, 1.0];
    for (g, e) in got.iter().zip(expected.iter()) {
        assert!(
            (g - e).abs() < 1e-6,
            "dequant cell {got:?} vs expected {expected:?}: component diff {} too large",
            (g - e).abs()
        );
    }
}
