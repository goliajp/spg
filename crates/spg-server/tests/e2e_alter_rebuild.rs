//! v6.0.4 — `ALTER INDEX <name> REBUILD [WITH (encoding = ...)]`.
//!
//! Synchronous MVP scope (see `V6_DESIGN.md::L3a-v6.0.4`): rebuild
//! holds the engine write-lock for the duration; "live" non-
//! blocking rebuild lands as v6.0.4.1 / v6.1.x. These tests drive
//! the full stack through pgwire to confirm the SQL surface +
//! cell re-encoding work end-to-end.

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

fn ingest_topk_corpus(s: &mut TcpStream, table: &str) {
    // Five rows whose L2 distance from [1, 2, 3, 4] (squared) is:
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
        send_query(s, &format!("INSERT INTO {table} VALUES ({id}, {v})"));
        expect_cc(s);
    }
}

#[test]
fn alter_rebuild_in_place_preserves_topk_order() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)",
    );
    expect_cc(&mut s);
    ingest_topk_corpus(&mut s, "emb");
    send_query(&mut s, "CREATE INDEX emb_idx ON emb USING hnsw (v)");
    expect_cc(&mut s);

    // ALTER … REBUILD without an encoding clause: same encoding,
    // fresh graph. Top-K order must stay identical.
    let pre = run_select(
        &mut s,
        "SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3",
    );
    send_query(&mut s, "ALTER INDEX emb_idx REBUILD");
    expect_cc(&mut s);
    let post = run_select(
        &mut s,
        "SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3",
    );
    assert_eq!(pre, post, "in-place rebuild must preserve kNN top-K order");
}

#[test]
fn alter_rebuild_with_encoding_switch_f32_to_sq8_recodes_cells() {
    // Start with a plain F32 column. ALTER REBUILD WITH
    // (encoding = SQ8) re-encodes every cell + rebuilds the graph.
    // Top-3 stays correct under SQ8 ADC + f32 rerank (the v6.0.1
    // search path now active for the migrated column).
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)",
    );
    expect_cc(&mut s);
    ingest_topk_corpus(&mut s, "emb");
    send_query(&mut s, "CREATE INDEX emb_idx ON emb USING hnsw (v)");
    expect_cc(&mut s);

    send_query(&mut s, "ALTER INDEX emb_idx REBUILD WITH (encoding = SQ8)");
    expect_cc(&mut s);

    let rows = run_select(
        &mut s,
        "SELECT id FROM emb ORDER BY v <-> [1.0, 2.0, 3.0, 4.0] LIMIT 3",
    );
    let ids: Vec<WireValue> = rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![WireValue::Int(1), WireValue::Int(5), WireValue::Int(4)],
        "post-rebuild SQ8 ORDER BY <-> must still match ground-truth top-3",
    );
}

#[test]
fn alter_rebuild_unknown_index_errors_on_wire() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "ALTER INDEX ghost REBUILD");
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = spg_wire::parse_error_response(&f).unwrap();
    assert!(
        msg.to_ascii_lowercase().contains("index not found") || msg.contains("ghost"),
        "expected index-not-found error, got {msg:?}"
    );
}
