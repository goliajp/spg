//! v7.17.0 Phase 2.3 — PG `statement_timeout` actually cancels.
//!
//! Pre-Phase 2.3 the GUC parsed + SHOWed but nothing ticked. These
//! tests drive the pgwire path end-to-end: SET the timeout, run a
//! query big enough to outlast it, and assert the server replies
//! with PG SQLSTATE `57014` (`query_canceled`) + the canonical
//! "canceling statement due to statement timeout" text.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-stmt-timeout-{nanos}"));
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
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
}

fn drain_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = drain_until_ready(&mut s);
    s
}

/// Extract a tagged field from a PG ErrorResponse body. Fields are
/// `[u8 tag][C-string value]` runs terminated by a NUL byte.
/// `tag` examples: b'S' = severity, b'C' = SQLSTATE, b'M' = message.
fn error_field(msgs: &[PgMessage], tag: u8) -> Option<String> {
    let err = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < err.body.len() {
        let t = err.body[i];
        if t == 0 {
            return None;
        }
        i += 1;
        let mut end = i;
        while end < err.body.len() && err.body[end] != 0 {
            end += 1;
        }
        let val = std::str::from_utf8(&err.body[i..end]).ok()?.to_string();
        if t == tag {
            return Some(val);
        }
        i = end + 1;
    }
    None
}

fn first_cell(msgs: &[PgMessage]) -> String {
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    assert!(len >= 0, "expected non-null cell, got len={len}");
    let start = 6;
    let end = start + len as usize;
    std::str::from_utf8(&dr.body[start..end])
        .unwrap()
        .to_string()
}

#[test]
fn statement_timeout_zero_disables() {
    // Default value `0` means "no timeout". A fast query under
    // a 0-timeout session must complete normally; if Phase 2.3
    // ever regressed and started arming a deadline at 0, this
    // test catches it.
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET statement_timeout = '0'");
    let msgs = drain_until_ready(&mut s);
    assert!(msgs.iter().any(|m| m.ty == b'C'), "expected CC for SET");

    send_query(&mut s, "CREATE TABLE t (n INT)");
    let _ = drain_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1), (2), (3)");
    let _ = drain_until_ready(&mut s);
    send_query(&mut s, "SELECT n FROM t");
    let msgs = drain_until_ready(&mut s);
    let rows = msgs.iter().filter(|m| m.ty == b'D').count();
    assert_eq!(rows, 3);
}

#[test]
fn statement_timeout_quick_query_under_budget_succeeds() {
    // 30 second budget, near-instant query — must complete.
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET statement_timeout = '30s'");
    let _ = drain_until_ready(&mut s);

    send_query(&mut s, "CREATE TABLE small (n INT)");
    let _ = drain_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO small VALUES (1), (2), (3)");
    let _ = drain_until_ready(&mut s);
    send_query(&mut s, "SELECT n FROM small");
    let msgs = drain_until_ready(&mut s);
    assert!(
        msgs.iter().any(|m| m.ty == b'D'),
        "30s timeout must not cancel a trivial SELECT"
    );
}

#[test]
fn statement_timeout_fires_57014_on_long_update() {
    // Seed a big table, then `SET statement_timeout = '1ms'` and
    // UPDATE every row. UPDATE checks the cancel token every 256
    // rows (dml.rs), so the 1ms budget trips mid-scan and the
    // statement is canceled at ~1ms — the test stays fast.
    //
    // Determinism note: the failure mode this guards against is the
    // UPDATE *completing* before the 1ms deadline. That is a race
    // only when the work ≈ the timeout. 1ms is the smallest non-zero
    // statement_timeout (PG takes integer ms), so the margin has to
    // come from the workload: 100k single-int updates run ~20ms
    // uninterrupted on a current M-series, a 20x margin over 1ms that
    // no plausible machine-speed or scheduling jitter closes. (The
    // old 5000-row seed took ~1ms — exactly the timeout — so a fast
    // host finished before the deadline and the cancel never fired.)
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "CREATE TABLE big (n INT)");
    let _ = drain_until_ready(&mut s);

    // 100k rows, inserted in 1000-row batches so no single statement
    // is pathologically large for the parser. Seeding runs before the
    // timeout is set, so its cost doesn't enter the measured window.
    for batch in 0..100 {
        let mut sql = String::from("INSERT INTO big VALUES ");
        for j in 0..1000 {
            if j > 0 {
                sql.push(',');
            }
            sql.push('(');
            sql.push_str(&(batch * 1000 + j).to_string());
            sql.push(')');
        }
        send_query(&mut s, &sql);
        let _ = drain_until_ready(&mut s);
    }

    send_query(&mut s, "SET statement_timeout = '1ms'");
    let _ = drain_until_ready(&mut s);

    send_query(&mut s, "UPDATE big SET n = n + 1");
    let msgs = drain_until_ready(&mut s);

    let sqlstate =
        error_field(&msgs, b'C').expect("expected ErrorResponse with SQLSTATE on timeout");
    assert_eq!(
        sqlstate, "57014",
        "PG `statement_timeout` must surface as query_canceled"
    );
    let message = error_field(&msgs, b'M').unwrap_or_default();
    assert!(
        message.contains("canceling statement due to statement timeout"),
        "ErrorResponse text must match PG's canonical phrasing; got {message:?}"
    );
}

#[test]
fn statement_timeout_show_round_trip() {
    // Regression: the GUC must still round-trip through SHOW.
    // v7.39 (GUC) — the engine session store is consulted first and
    // ms-unit time GUCs render PG-style in the largest whole unit:
    // PG18 shows `SET statement_timeout = '2500'` back as "2500ms"
    // (2500 % 1000 != 0), verified against the live oracle.
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET statement_timeout = '2500'");
    let _ = drain_until_ready(&mut s);

    send_query(&mut s, "SHOW statement_timeout");
    let msgs = drain_until_ready(&mut s);
    assert_eq!(first_cell(&msgs), "2500ms");
}
