//! v7.39 (round 304, V38) — the WAL fsync gate must witness THIS
//! connection's transaction, not the engine's global one.
//!
//! `persist_wire_write` decides whether a write's WAL append fsyncs
//! before the ack. The intent is PG's rule: inside an explicit
//! transaction don't fsync per statement, fsync once at COMMIT. The
//! witness it used was `Engine::in_transaction()` — which is
//! `!tx_catalogs.is_empty()`, i.e. true whenever ANY connection holds a
//! transaction. So an autocommit write on connection A skipped its
//! fsync merely because connection B had a transaction open, and A was
//! acked before its bytes were durable. Under `synchronous_commit = on`
//! that is a broken contract.
//!
//! This predates the round: r283 gave pgwire per-connection tx slots,
//! and before that only one transaction could exist at a time, so the
//! global check and the per-slot check agreed. Sibling instances of the
//! same global-vs-slot confusion were fixed one at a time (r298's
//! aborted flag, pgwire's streaming `conn_in_tx`); the fsync gate was
//! the one left.
//!
//! Instrument: `SPG_FAIL_FSYNC_AT=K` makes the K-th client-path
//! `sync_data` fail once. A write that fsyncs surfaces that as a client
//! error; a write that skips the fsync succeeds silently. That
//! difference is the whole test — no crash simulation needed, because
//! the defect is "acked without fsync", which this observes directly.
//! (`kill -9` could not show it anyway: the bytes are always
//! `write_all`'d, so only power loss would actually drop them.)

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-fsync-gtx-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

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

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
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
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
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

/// `Some(message)` when the statement answered an ErrorResponse.
fn run(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    msgs.iter().find(|m| m.ty == b'E').map(|m| {
        let mut text = String::new();
        for field in m.body.split(|b| *b == 0) {
            if field.first() == Some(&b'M') {
                text = String::from_utf8_lossy(&field[1..]).into_owned();
            }
        }
        text
    })
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

/// A WAL-backed server with the fsync chaos knob armed.
fn boot(label: &str, fail_at: &str) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .env("SPG_FAIL_FSYNC_AT", fail_at)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    (common::ChildGuard(raw), addr)
}

// NOTE on statement choice: plain autocommit DML does NOT reach
// `persist_wire_write` — `try_queue_plain_dml` diverts it to the
// commit-barrier group path, which fsyncs on its own (r178). DDL is not
// queue-eligible, so it takes `execute_with_role` + `persist_wire_write`
// — the path whose fsync gate is under test here.

/// Baseline: with nobody else in a transaction, an autocommit DDL write
/// fsyncs, so the injected failure reaches the client. Also calibrates
/// the numbering the V38 test relies on (first CREATE = fsync #1).
#[test]
fn autocommit_ddl_fsyncs_when_no_one_holds_a_transaction() {
    let (_guard, addr) = boot("control", "2");
    let mut a = open(&addr);
    assert_eq!(run(&mut a, "CREATE TABLE t1 (id int)"), None, "fsync #1");
    let err = run(&mut a, "CREATE TABLE t2 (id int)");
    assert!(
        err.is_some(),
        "the second DDL must fsync, so the injected failure must surface \
         as a client error — got a silent success instead"
    );
}

/// V38: another connection holding a transaction must not suppress this
/// connection's fsync. Same script as the baseline, with connection B
/// sitting inside `BEGIN`. If the gate consults the engine-global
/// `in_transaction()`, A's DDL skips its fsync and is acked anyway — the
/// injected failure never fires and this assertion fails.
#[test]
fn another_connections_transaction_does_not_suppress_our_fsync() {
    let (_guard, addr) = boot("global", "2");
    let mut a = open(&addr);
    assert_eq!(run(&mut a, "CREATE TABLE t1 (id int)"), None, "fsync #1");

    // B opens a transaction and just sits there. `BEGIN` appends to the
    // WAL without fsyncing, so it consumes no fsync of its own.
    let mut b = open(&addr);
    assert_eq!(run(&mut b, "BEGIN"), None);

    let err = run(&mut a, "CREATE TABLE t2 (id int)");
    assert!(
        err.is_some(),
        "A's autocommit write must still fsync while B merely holds a \
         transaction; a silent success means the fsync gate is reading \
         the engine-global in_transaction() instead of A's own slot (V38)"
    );
}

/// The other half of the contract r177 established, which the V38 fix
/// must not undo: inside a connection's OWN transaction, statements
/// append without fsyncing — the COMMIT pays for the whole block. Losing
/// this would put an fsync back on every statement of a transaction
/// (r177 measured that at 100 fsyncs ≈ 740 ms for a 100-statement tx).
#[test]
fn statements_inside_our_own_transaction_still_defer_the_fsync() {
    let (_guard, addr) = boot("intx", "2");
    let mut a = open(&addr);
    assert_eq!(run(&mut a, "CREATE TABLE t1 (id int)"), None, "fsync #1");
    assert_eq!(run(&mut a, "BEGIN"), None);
    assert_eq!(
        run(&mut a, "CREATE TABLE t2 (id int)"),
        None,
        "a statement inside our own transaction must NOT fsync — if it \
         did, the injected failure at #2 would have surfaced here"
    );
    // Leaving the transaction is the fsync point, so #2 lands here.
    let err = run(&mut a, "COMMIT");
    assert!(
        err.is_some(),
        "COMMIT must fsync once for the whole transaction"
    );
}
