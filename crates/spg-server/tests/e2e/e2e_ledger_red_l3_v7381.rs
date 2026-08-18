//! 7.38.1 S0.1 — L3 red pin (D29 residual), checked in AS a red.
//!
//! The native protocol's NON-WRAP branch (a write inside an explicit
//! transaction) still audits POST-HOC: apply first, append the audit
//! entry after. When the append fails, the client gets an error for a
//! statement that took effect — the error-but-applied shape D29
//! removed from the wrap path. Un-ignore in 7.38.1 S3.2.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, build_query, encode};

const READ_TIMEOUT: Duration = Duration::from_secs(5);
static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-l3red-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn drain_result(stream: &mut TcpStream) -> Op {
    // First frame decides; drain any row stream to CommandComplete.
    let f = read_frame(stream);
    match f.op {
        Op::RowDescription => {
            let mut last = f.op;
            while last != Op::CommandComplete {
                last = read_frame(stream).op;
            }
            Op::CommandComplete
        }
        other => other,
    }
}

/// error ⇒ no effect, on the explicit-TX native path too.
#[test]
#[ignore = "7.38.1 L3 red (D29 residual) — un-ignore in S3.2"]
fn l3_failed_audit_in_explicit_tx_must_not_apply() {
    let dir = unique_tmpdir();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .arg_path(&dir.join("audit.log"))
        .env("SPG_FAIL_AUDIT_AT", "1")
        .echo_stderr(true)
        .spawn();
    let mut _child = common::ChildGuard(raw);
    let mut stream = common::connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Probe finding (S0.1): on this path the per-statement writes
    // inside the tx are NOT audited — only the terminal COMMIT
    // appends one entry. So the armed failure lands on COMMIT.
    send_query(&mut stream, "BEGIN");
    assert_eq!(drain_result(&mut stream), Op::CommandComplete, "BEGIN");
    send_query(&mut stream, "CREATE TABLE l3t (v INT)");
    assert_eq!(drain_result(&mut stream), Op::CommandComplete, "CREATE");
    send_query(&mut stream, "COMMIT");
    // Today the server tears the CONNECTION down on the audit
    // failure (the handler returns Err) — an ErrorResponse may or
    // may not arrive first. Either way the client saw a failure;
    // both shapes are "the COMMIT errored". (PG keeps the session
    // alive on a statement error — the teardown itself is part of
    // this pin's fix scope.)
    let commit_failed =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain_result(&mut stream)))
            .map(|op| op == Op::ErrorResponse)
            .unwrap_or(true); // EOF/reset while reading = failed COMMIT too
    assert!(
        commit_failed,
        "the COMMIT's audit append must surface as a failure"
    );
    // A COMMIT the client was told FAILED must not have committed:
    // on a FRESH connection the table must be gone. Today it
    // survives — error-but-applied.
    let mut probe = common::connect_to(&addrs.native);
    probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut probe, "SELECT count(*) FROM l3t");
    let op = drain_result(&mut probe);
    assert_eq!(
        op,
        Op::ErrorResponse,
        "an errored COMMIT must roll back (error-but-applied residual)"
    );
}
