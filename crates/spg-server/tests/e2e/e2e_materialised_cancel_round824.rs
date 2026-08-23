//! r824 — a result the engine materialises can still be interrupted.
//!
//! `statement_timeout` and `CancelRequest` share one token, and the
//! streaming path checked it every 256 rows. Nothing else did. The loop
//! that hands an already-materialised result to its consumer existed
//! four times — in `exec_select_streaming`, twice more in the read-only
//! entry points, and once again in the arena path — and not one of the
//! four checked for cancellation.
//!
//! It looks like the cheap half of the work, because the rows already
//! exist. It is not: handing them over is what encodes them and pushes
//! them at the socket, and that is most of the elapsed time. Measured
//! over 600k rows of 200 bytes under a 120ms timeout, before the fix,
//! every shape the streaming path declines ran to completion — an
//! arithmetic projection, a function projection, `ORDER BY`, and a
//! subquery. So the two things a client can do about a runaway query
//! both did nothing, for precisely the shapes most likely to need them.
//!
//! Three of the four loops are one function now. The fourth could not
//! join them — its consumer takes columns and values rather than a
//! `StreamItem` — and it is the one that had to be found separately,
//! after the first three were fixed and one shape still would not stop.
//!
//! What these pin is the contract rather than the plumbing: each shape,
//! interrupted, ends in 57014. The second one additionally pins that the
//! check is reached BETWEEN rows and not merely before the loop, since
//! it cancels only after reading rows off the wire.
//!
//! That one deliberately does not use a deadline. Where a deadline lands
//! depends on how much of the result the kernel buffers and on how fast
//! the client reads, so aiming one at the emitting phase passed 2 runs
//! in 3 — a test that reports the defect it is meant to catch a third of
//! the time is worse than no test. A CancelRequest is sent in response
//! to something observed instead of at a guessed moment, which makes the
//! same assertion deterministic, and covers the other half of the token
//! while it is there.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(120);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn open(addr: &str) -> TcpStream {
    let (s, _) = open_with_key(addr);
    s
}

/// Like `open`, also returning the BackendKeyData a CancelRequest needs.
fn open_with_key(addr: &str) -> (TcpStream, (u32, u32)) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    // The handshake ends in its own ReadyForQuery; draining it with a
    // query would answer that query with the handshake's frames.
    let msgs = read_until_ready(&mut s);
    let k = msgs
        .iter()
        .find(|m| m.ty == b'K')
        .expect("BackendKeyData in the handshake");
    let pid = u32::from_be_bytes([k.body[0], k.body[1], k.body[2], k.body[3]]);
    let secret = u32::from_be_bytes([k.body[4], k.body[5], k.body[6], k.body[7]]);
    (s, (pid, secret))
}

/// A CancelRequest arrives on its own connection, as PG's protocol says.
fn send_cancel(addr: &str, (pid, secret): (u32, u32)) {
    let mut c = TcpStream::connect(addr).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&16_u32.to_be_bytes());
    out.extend_from_slice(&80_877_102_u32.to_be_bytes());
    out.extend_from_slice(&pid.to_be_bytes());
    out.extend_from_slice(&secret.to_be_bytes());
    c.write_all(&out).unwrap();
    // The server closes it without replying.
    let _ = c.read(&mut [0u8; 1]);
}

fn rows_of(msgs: &[PgMessage]) -> usize {
    msgs.iter().filter(|m| m.ty == b'D').count()
}

fn error_field(msgs: &[PgMessage], want: u8) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == want {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn sqlstate(msgs: &[PgMessage]) -> Option<String> {
    error_field(msgs, b'C')
}

fn message_of(msgs: &[PgMessage]) -> String {
    error_field(msgs, b'M').unwrap_or_default()
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = q(s, sql);
    assert_eq!(
        sqlstate(&msgs),
        None,
        "setup statement failed: {sql} -> {}",
        message_of(&msgs)
    );
    msgs
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = crate::common::tmp_base().join(format!("spg-e2e-matcancel-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

const ROWS: usize = 120_000;

/// Wide enough rows that pushing them costs real time, which is where
/// the missing checks were.
fn seed(s: &mut TcpStream) {
    ok(s, "CREATE TABLE big (id INT PRIMARY KEY, pad TEXT)");
    ok(
        s,
        &format!("INSERT INTO big SELECT g, repeat('y', 200) FROM generate_series(1,{ROWS}) g"),
    );
}

/// Read `want` rows off an in-flight result, then cancel it and drain.
/// Returns (rows delivered, SQLSTATE, saw CommandComplete).
fn cancel_after_rows(
    addr: &str,
    s: &mut TcpStream,
    key: (u32, u32),
    sql: &str,
    want: usize,
) -> (usize, Option<String>, bool) {
    send_query(s, sql);
    let mut delivered = 0usize;
    while delivered < want {
        let m = read_message(s);
        match m.ty {
            b'D' => delivered += 1,
            b'E' => panic!("`{sql}` errored before {want} rows: {}", message_of(&[m])),
            b'Z' => panic!("`{sql}` finished before {delivered} rows could be read"),
            _ => {}
        }
    }
    send_cancel(addr, key);
    let mut code = None;
    let mut complete = false;
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => delivered += 1,
            b'C' => complete = true,
            b'E' => code = sqlstate(&[m]),
            b'Z' => break,
            _ => {}
        }
    }
    (delivered, code, complete)
}

#[test]
fn every_materialising_shape_can_be_interrupted_once_its_rows_are_flowing() {
    let (_child, addrs) = spawn("shapes");
    let addr = addrs.pgwire.as_ref().unwrap();
    let (mut s, key) = open_with_key(addr);
    seed(&mut s);

    // Each of these was declined by the streaming path for its own reason
    // when this was written, and each reached a different one of the four
    // loops that had no check. Round 831 gave joinless single-table
    // SELECTs a streaming walk, so the arithmetic and function
    // projections now take that instead — and are still interrupted,
    // by the check in the new walk. What the list pins is the contract,
    // which holds whichever path a shape ends up on; the subquery is the
    // one that goes through pgwire's own materialised loop.
    let shapes = [
        "SELECT id + 0 FROM big",
        "SELECT upper(pad) FROM big",
        "SELECT pad FROM big ORDER BY id",
        "SELECT pad FROM big WHERE id IN (SELECT id FROM big)",
    ];

    for sql in shapes {
        let (delivered, code, complete) = cancel_after_rows(addr, &mut s, key, sql, 200);
        assert_eq!(
            code.as_deref(),
            Some("57014"),
            "`{sql}` was cancelled with rows already on the wire and \
             delivered all {delivered} of them anyway"
        );
        assert!(!complete, "`{sql}` reported CommandComplete when cancelled");
        assert!(
            delivered < ROWS,
            "`{sql}` delivered all {ROWS} rows and then reported 57014"
        );
    }

    // The session survives all four cancellations, on the same connection.
    let alive = ok(&mut s, "SELECT count(*) FROM big");
    assert_eq!(rows_of(&alive), 1);
}

#[test]
fn the_check_is_reached_between_rows_not_only_before_the_loop() {
    let (_child, addrs) = spawn("midemit");
    let addr = addrs.pgwire.as_ref().unwrap();
    let (mut s, key) = open_with_key(addr);
    seed(&mut s);

    // Same construction, stated as its own contract: rows are read off
    // the wire first, so the emitting loop is demonstrably running when
    // the cancel arrives. A check that ran only before the loop could
    // not end this statement at all, and one that ran only after it
    // would let all 120000 rows through first.
    let (delivered, code, complete) =
        cancel_after_rows(addr, &mut s, key, "SELECT id + 0 FROM big", 200);

    assert_eq!(code.as_deref(), Some("57014"));
    assert!(!complete);
    assert!(
        delivered >= 200,
        "the rows read before cancelling must stay delivered"
    );
    assert!(
        delivered < ROWS,
        "delivered all {ROWS} rows and then reported 57014"
    );

    // The session survives, on the connection that was cancelled.
    let alive = ok(&mut s, "SELECT count(*) FROM big");
    assert_eq!(rows_of(&alive), 1);
}
