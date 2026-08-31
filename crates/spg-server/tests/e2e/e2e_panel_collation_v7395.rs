//! v7.39.5 — the wire panel's collation is DECLARED, and this is the
//! fixture that can tell the two declarations apart.
//!
//! Until this version `ServerBuilder` declared nothing: it clears three
//! variables and inherits the rest, so the collation of every server in
//! this panel was whatever the operator's shell said. Both of this
//! project's machines export `LANG=en_US.UTF-8`, so the panel had been
//! ordering text by a locale while every fixture in it was written
//! under `C` — and a CI runner with `LANG` unset was running a
//! different panel from the one anybody had looked at.
//!
//! Declaring it is only half. Running the panel twice is worth nothing
//! if no fixture can distinguish the two runs, and when this was
//! measured — 734 wire tests under `C`, then all 734 under
//! `en_US.utf8` — the two agreed everywhere. So this file is the one
//! that disagrees: it asks for an ordering the two collations answer
//! differently, and expects the answer belonging to the panel it is
//! running in.
//!
//! `Bob,Zebra,apple` by bytes, because every capital sorts before every
//! lowercase; `apple,Bob,Zebra` by `en_US`, which orders letters and
//! then case. Same rows, same query, same server.

use crate::common;
use std::io::Write;
use std::net::TcpStream;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_data_row,
    parse_data_row_batch,
};

fn read_frame(stream: &mut TcpStream) -> Frame {
    use std::io::Read;
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn send(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::RowDescription => {}
            Op::DataRow => rows.push(parse_data_row(&f).expect("row")),
            Op::DataRowBatch => rows.extend(parse_data_row_batch(&f).expect("batch")),
            Op::CommandComplete => break,
            other => panic!("{sql}: unexpected {other:?}"),
        }
    }
    rows
}

fn column(stream: &mut TcpStream, sql: &str) -> Vec<String> {
    send(stream, sql)
        .iter()
        .map(|r| match r.first() {
            Some(WireValue::Text(t)) => t.clone(),
            other => panic!("{sql}: expected text, got {other:?}"),
        })
        .collect()
}

fn scalar(stream: &mut TcpStream, sql: &str) -> String {
    match send(stream, sql).first().and_then(|r| r.first()) {
        Some(WireValue::Text(t)) => t.clone(),
        other => panic!("{sql}: expected text, got {other:?}"),
    }
}

#[test]
fn the_panel_orders_text_the_way_it_declares() {
    let (child, addrs) = common::ServerBuilder::new().spawn();
    let mut c = common::ChildGuard(child);
    let mut s = common::connect_to(&addrs.native);

    // The server's own answer about itself, first: if the declaration
    // did not reach the child, the ordering below would be judged
    // against a panel this test is not in.
    let declared = scalar(
        &mut s,
        "SELECT datcollate FROM pg_database WHERE datname = current_database()",
    );
    let expected_declared = common::panel_collation();
    assert!(
        declared.eq_ignore_ascii_case(&expected_declared),
        "the panel declares {expected_declared:?} but the server says {declared:?} — \
         the declaration did not reach the child"
    );

    send(&mut s, "CREATE TABLE p (w TEXT)");
    for w in ["Zebra", "apple", "Bob"] {
        send(&mut s, &format!("INSERT INTO p VALUES ('{w}')"));
    }
    let got = column(&mut s, "SELECT w FROM p ORDER BY w");

    let expected: Vec<&str> = if common::panel_is_collated() {
        vec!["apple", "Bob", "Zebra"]
    } else {
        vec!["Bob", "Zebra", "apple"]
    };
    assert_eq!(
        got, expected,
        "panel collation {expected_declared:?}: ORDER BY answered {got:?}"
    );

    // And the two answers really are different, so a panel that silently
    // fell back to the other collation cannot pass this by agreeing with
    // both.
    let other: Vec<&str> = if common::panel_is_collated() {
        vec!["Bob", "Zebra", "apple"]
    } else {
        vec!["apple", "Bob", "Zebra"]
    };
    assert_ne!(
        expected, other,
        "the fixture must distinguish the two panels"
    );

    drop(s);
    c.0.kill().ok();
}
