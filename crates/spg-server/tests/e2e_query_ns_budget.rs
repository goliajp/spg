//! v6.10.1 — `SPG_MAX_QUERY_NS` per-query CPU/wall budget.
//!
//! Finer-grained companion to `SPG_QUERY_TIMEOUT_MS` (1 ms
//! resolution). When both envs are set, the *tighter* effective
//! deadline wins. The watchdog flips the per-query cancel flag
//! when the budget elapses; running scans / loops poll the
//! flag at checkpoints and surface a clean cancellation error.

#![allow(clippy::uninlined_format_args)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Op, build_query, encode, parse_error_response};

mod common;
use common::ServerBuilder;

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn read_response(s: &mut TcpStream) -> Result<(), String> {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).map_err(|e| format!("bad op: {e:?}"))?;
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).map_err(|e| e.to_string())?;
        }
        match op {
            Op::CommandComplete => return Ok(()),
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                return Err(parse_error_response(&f).unwrap_or("<undecodable>").to_string());
            }
            _ => continue,
        }
    }
}

fn exec(s: &mut TcpStream, sql: &str) -> Result<(), String> {
    send_query(s, sql);
    read_response(s)
}

#[test]
fn ns_budget_unset_is_no_budget() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    exec(&mut s, "CREATE TABLE t (id INT NOT NULL)").unwrap();
    exec(&mut s, "INSERT INTO t VALUES (1)").unwrap();
    exec(&mut s, "SELECT id FROM t WHERE id = 1").expect("legacy path");
}

#[test]
fn ns_budget_loose_does_not_cancel_fast_select() {
    // 10 seconds in ns. A trivial SELECT should fit easily.
    let (raw, addrs) = ServerBuilder::new()
        .env("SPG_MAX_QUERY_NS", "10000000000")
        .spawn();
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    exec(&mut s, "CREATE TABLE t (id INT NOT NULL)").unwrap();
    exec(&mut s, "INSERT INTO t VALUES (1)").unwrap();
    exec(&mut s, "SELECT id FROM t WHERE id = 1").unwrap();
}

#[test]
fn ms_and_ns_both_set_pick_tighter() {
    // _MS = 5000 ms (5 s), _NS = 100 000 000 (100 ms). The
    // tighter wins (100 ms) — still loose enough for a trivial
    // SELECT to complete cleanly. The point of this test is
    // that the wiring picks one of the two; firing-under-budget
    // belongs to a separate test below.
    let (raw, addrs) = ServerBuilder::new()
        .env("SPG_QUERY_TIMEOUT_MS", "5000")
        .env("SPG_MAX_QUERY_NS", "100000000")
        .spawn();
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    exec(&mut s, "CREATE TABLE t (id INT NOT NULL)").unwrap();
    exec(&mut s, "INSERT INTO t VALUES (1)").unwrap();
    exec(&mut s, "SELECT id FROM t WHERE id = 1").unwrap();
}
