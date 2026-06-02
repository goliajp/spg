#![allow(unused_mut, unused_variables)]
//! v5.5.1 per-query memory budget (`SPG_MAX_QUERY_BYTES`) end-to-end:
//! - a SELECT whose result materialises past the ceiling is cancelled with a
//!   clear error instead of driving the process toward the OOM killer;
//! - a normal sub-budget query is unaffected (no false positives);
//! - the per-query reset means a long load of small INSERTs never trips, even
//!   though their cumulative bytes dwarf the ceiling.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, Op, build_query, encode, parse_error_response};

mod common;

fn local_spawn(envs: &[(&str, &str)]) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new();
    for (k, v) in envs {
        b = b.env(*k, *v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 1 MiB ceiling: small enough that a few thousand padded rows blow past it,
/// large enough that parsing/planning a normal query stays well under.
const BUDGET: &str = "1048576";

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

fn send(s: &mut TcpStream, f: &Frame) {
    let mut buf = Vec::new();
    encode(f, &mut buf).unwrap();
    s.write_all(&buf).unwrap();
}

/// CREATE the table and bulk-load `batches * 100` padded rows. Each INSERT
/// batch is ~40 KB — far under the ceiling — and the budget resets per query,
/// so the load never trips it even though the total is multiples of the cap.
fn create_and_load(s: &mut TcpStream, batches: usize) {
    use std::fmt::Write as _;
    send(
        s,
        &build_query("CREATE TABLE t (id INT NOT NULL, payload TEXT NOT NULL)"),
    );
    assert_eq!(read_frame(s).op, Op::CommandComplete);
    let pad = "x".repeat(400);
    for batch in 0..batches {
        let mut sql = String::from("INSERT INTO t VALUES ");
        for row in 0..100 {
            let id = batch * 100 + row + 1;
            if row > 0 {
                sql.push(',');
            }
            write!(sql, "({id},'{pad}')").unwrap();
        }
        send(s, &build_query(&sql));
        assert_eq!(
            read_frame(s).op,
            Op::CommandComplete,
            "load batch {batch} should commit under the per-query budget"
        );
    }
}

#[test]
fn over_budget_select_is_cancelled() {
    let (raw, addrs) = local_spawn(&[("SPG_MAX_QUERY_BYTES", BUDGET)]);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // 8000 rows × 400-char payload ≈ 3.2 MB of result data — ~3× the 1 MiB
    // ceiling, so the SELECT's materialisation crosses it with ample margin.
    create_and_load(&mut s, 80);

    send(&mut s, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s);
    assert_eq!(
        f.op,
        Op::ErrorResponse,
        "over-budget SELECT must error, not stream the whole table"
    );
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.to_lowercase().contains("cancel"),
        "expected a cancellation error from the memory budget, got {msg:?}"
    );
}

#[test]
fn under_budget_select_succeeds() {
    let (raw, addrs) = local_spawn(&[("SPG_MAX_QUERY_BYTES", BUDGET)]);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // 200 rows × 400 char ≈ 80 KB ≪ 1 MiB — comfortably under budget.
    create_and_load(&mut s, 2);

    send(&mut s, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s);
    assert_eq!(
        f.op,
        Op::RowDescription,
        "under-budget SELECT should stream normally, not get cancelled"
    );
}

/// v5.5.2 OOM-as-clean-error invariant. Under `panic = "abort"` a true malloc
/// failure cannot be unwound into an error, and `set_alloc_error_hook` is
/// nightly-only — so the per-query budget IS the clean-error path: it cancels a
/// runaway query *before* it reaches a real OOM. This chaos test applies
/// repeated over-budget pressure and asserts the invariant the ship gate cares
/// about: every attempt comes back as a cancellation, and the server process
/// never aborts/panics — it keeps serving.
#[test]
fn chaos_oom_returns_cancelled_not_panic() {
    let (raw, addrs) = local_spawn(&[("SPG_MAX_QUERY_BYTES", BUDGET)]);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // ~3.2 MB table — 3× the 1 MiB ceiling.
    create_and_load(&mut s, 80);

    // A tiny separate table for the survival probe. spg applies LIMIT *after*
    // materialisation, so `SELECT * FROM t LIMIT 1` would itself scan the whole
    // 3.2 MB table and get cancelled — not a valid liveness check. Querying a
    // one-row table never approaches the budget.
    send(&mut s, &build_query("CREATE TABLE probe (id INT NOT NULL)"));
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    send(&mut s, &build_query("INSERT INTO probe VALUES (1)"));
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);

    // Repeated OOM pressure: each over-budget SELECT must come back as a clean
    // cancellation, not crash the server.
    for i in 0..5 {
        send(&mut s, &build_query("SELECT * FROM t"));
        let f = read_frame(&mut s);
        assert_eq!(
            f.op,
            Op::ErrorResponse,
            "iteration {i}: over-budget SELECT must be cancelled, not stream"
        );
        let msg = parse_error_response(&f).unwrap();
        assert!(
            msg.to_lowercase().contains("cancel"),
            "iteration {i}: expected a cancellation error, got {msg:?}"
        );
    }

    // Survival proof: a normal query on the same connection still works, so the
    // process never aborted under the repeated pressure.
    send(&mut s, &build_query("SELECT * FROM probe"));
    let f = read_frame(&mut s);
    assert_eq!(
        f.op,
        Op::RowDescription,
        "server must survive repeated OOM pressure and keep serving"
    );

    // And the child is still alive (not aborted).
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "server process must not have aborted under OOM pressure"
    );
}
