//! v7.40.4 — the parallel sort, pinned OVER THE WIRE.
//!
//! This file exists because its in-process twin was not enough, and the
//! way that came out is worth the space.
//!
//! `crates/spg-engine/tests/e2e/e2e_sort_parallel_v7404.rs` builds an
//! `Engine` and asks it for an `ORDER BY`. Under ablation it reddens, so
//! the parallel sort is reached — by that engine. The server is a
//! different route: an autocommit `SELECT` over the wire takes
//! `execute_readonly_select_streaming` ->
//! `try_exec_joined_streaming` -> `try_spill_sorted_stream` ->
//! `extsort.rs`, and NONE of the four sorts the first version wired are
//! on it. A server built with every one of them replaced by "return the
//! input untouched" answered `SELECT k FROM sw ORDER BY k` over 400,000
//! rows correctly sorted, at three rows too. The pins were green and the
//! feature was not on the customer's path.
//!
//! So this one talks to a real `spg-server` over pgwire. The rule it
//! stands for: a pin on an `Engine` is not a pin on the server, and the
//! release panel measures the server.
//!
//! What it asks is the same thing: the same query with the workers off
//! and on, row for row. That is safe to demand because the comparators
//! are strict total orders or stable sorts, so splitting cannot reach a
//! different answer — see `spg_engine::parsort`.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Above `parsort::MIN_PARALLEL` (65,536), and no further: every row
/// past it is time this suite spends proving something the engine's own
/// unit tests already prove.
const N: i64 = 70_000;

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-sortpar-wire-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
}

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// Run one simple-Query statement and return the first column of every
/// row it produced. An `ErrorResponse` fails the test where it happens,
/// rather than becoming an empty answer two assertions later.
fn rows(s: &mut TcpStream, sql: &str) -> Vec<String> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut got: Vec<String> = Vec::new();
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                // Int16 field count, then per field Int32 length + bytes.
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                assert!(n >= 1, "{sql}: a DataRow with no fields");
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                got.push(if len < 0 {
                    String::new()
                } else {
                    let end = 6 + len as usize;
                    String::from_utf8_lossy(&body[6..end]).into_owned()
                });
            }
            b'E' => {
                let mut msg = String::new();
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
                    if tag == b'M' {
                        msg = String::from_utf8_lossy(&body[pos..end]).into_owned();
                    }
                    pos = end + 1;
                }
                panic!("{sql}: {msg}");
            }
            b'Z' => return got,
            _ => {}
        }
    }
}

fn seeded() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    rows(&mut s, "CREATE TABLE t (k int, s text)");
    // `g*7919 mod N` is a permutation of `0..N`, so no sort sees its
    // input in order.
    rows(
        &mut s,
        &format!(
            "INSERT INTO t SELECT (g*7919)%{N}, 'k' || lpad(((g*7919)%{N})::text, 8, '0') \
             FROM generate_series(1,{N}) g"
        ),
    );
    let n = rows(&mut s, "SELECT count(*) FROM t");
    assert_eq!(n, [N.to_string()], "the fixture did not land");
    (raw, s)
}

#[test]
fn the_split_does_not_change_what_the_wire_returns() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    // `work_mem` small enough that this takes the spilling sorter, which
    // is the one an autocommit ORDER BY actually runs.
    rows(&mut s, "SET work_mem = 4096");
    for sql in [
        "SELECT k FROM t ORDER BY k",
        "SELECT s FROM t ORDER BY s",
        "SELECT k FROM t ORDER BY k DESC",
        "SELECT k FROM t ORDER BY s, k",
    ] {
        rows(&mut s, "SET max_parallel_workers_per_gather = 0");
        let serial = rows(&mut s, sql);
        assert_eq!(
            serial.len(),
            usize::try_from(N).unwrap(),
            "{sql}: the fixture must be large enough to reach the parallel path"
        );
        for setting in ["1", "2", "3", "7", "64"] {
            rows(
                &mut s,
                &format!("SET max_parallel_workers_per_gather = {setting}"),
            );
            assert_eq!(
                serial,
                rows(&mut s, sql),
                "{sql}: max_parallel_workers_per_gather = {setting} changed the answer"
            );
        }
        // Reproducible is not the same as right.
        if sql == "SELECT k FROM t ORDER BY k" {
            let got: Vec<i64> = serial.iter().map(|x| x.parse().unwrap()).collect();
            let mut want = got.clone();
            want.sort_unstable();
            assert_eq!(got, want, "ascending is not in order");
        }
        if sql == "SELECT k FROM t ORDER BY k DESC" {
            let got: Vec<i64> = serial.iter().map(|x| x.parse().unwrap()).collect();
            let mut want = got.clone();
            want.sort_unstable_by(|a, b| b.cmp(a));
            assert_eq!(got, want, "descending is not in order");
        }
    }
}
