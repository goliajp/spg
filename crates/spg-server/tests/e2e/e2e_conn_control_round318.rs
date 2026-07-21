//! read01 round 318 (V51 / V41) — naming a connection is only half of it;
//! this is the half that acts.
//!
//! `pg_cancel_backend` / `pg_terminate_backend` used to return `true`
//! unconditionally and do nothing, so an operator (or a supervisor script)
//! was told a runaway connection had been dealt with while it kept running.
//! Every expectation below was read off live PG 18.4:
//!
//!   * an id that is not a live backend → `f` plus
//!     `WARNING: PID N is not a PostgreSQL backend process`
//!   * terminating a live backend → `t`, and that backend receives
//!     `FATAL: terminating connection due to administrator command`
//!     (SQLSTATE 57P01) and is closed.
//!
//! The WARNING is also the first user of the notice channel's severity
//! (V41) — it used to be hardcoded NOTICE.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-conncontrol-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

/// Read one message; `None` on a closed connection.
fn read_message(s: &mut TcpStream) -> Option<PgMessage> {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).ok()?;
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).ok()?;
    }
    Some(PgMessage { ty, body })
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
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
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

/// Collect messages up to ReadyForQuery, or until the socket closes.
fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    while let Some(m) = read_message(s) {
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            break;
        }
    }
    out
}

fn datarow_cell(body: &[u8], col_idx: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col_idx >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col_idx {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col_idx {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

/// Split an ErrorResponse / NoticeResponse body into its `code → value`
/// fields (`S` severity, `C` SQLSTATE, `M` message, …).
fn fields(body: &[u8]) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let code = body[p];
        p += 1;
        let end = body[p..].iter().position(|&b| b == 0).unwrap() + p;
        out.push((code, String::from_utf8_lossy(&body[p..end]).into_owned()));
        p = end + 1;
    }
    out
}

fn field_of(m: &PgMessage, code: u8) -> Option<String> {
    fields(&m.body)
        .into_iter()
        .find_map(|(c, v)| (c == code).then_some(v))
}

fn open(addr: &str, user: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    let _ = read_until_ready(&mut s);
    s
}

fn scalar(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    msgs.iter()
        .find(|m| m.ty == b'D')
        .and_then(|m| datarow_cell(&m.body, 0))
        .unwrap_or_else(|| panic!("no row for `{sql}`"))
}

fn spawn() -> (common::ChildGuard, String) {
    let db = unique_tmpdir("svc").join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.expect("pgwire addr");
    (common::ChildGuard(child), addr)
}

/// An id no live backend carries: `f`, plus PG's warning. Answering `t`
/// (as SPG did) tells a supervisor the runaway backend is gone.
#[test]
fn signalling_an_unknown_pid_answers_false_with_a_warning() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr, "alice");

    for fname in ["pg_cancel_backend", "pg_terminate_backend"] {
        send_query(&mut s, &format!("SELECT {fname}(999999)"));
        let msgs = read_until_ready(&mut s);
        let val = msgs
            .iter()
            .find(|m| m.ty == b'D')
            .and_then(|m| datarow_cell(&m.body, 0))
            .expect("a row");
        assert_eq!(val, "f", "{fname} on an unknown pid must answer false");

        let notice = msgs
            .iter()
            .find(|m| m.ty == b'N')
            .unwrap_or_else(|| panic!("{fname} must raise PG's warning"));
        assert_eq!(
            field_of(notice, b'S').as_deref(),
            Some("WARNING"),
            "severity must be WARNING, not NOTICE"
        );
        assert_eq!(
            field_of(notice, b'V').as_deref(),
            Some("WARNING"),
            "the non-localized severity field too"
        );
        assert_eq!(
            field_of(notice, b'M').as_deref(),
            Some("PID 999999 is not a PostgreSQL backend process"),
        );
    }
}

/// Terminating a live backend answers `t` AND closes it, with PG's FATAL.
#[test]
fn terminating_a_live_backend_closes_it_with_a_fatal() {
    let (_guard, addr) = spawn();
    let mut killer = open(&addr, "killer");
    let mut victim = open(&addr, "victim");
    let victim_pid = scalar(&mut victim, "SELECT pg_backend_pid()");

    let answer = scalar(
        &mut killer,
        &format!("SELECT pg_terminate_backend({victim_pid})"),
    );
    assert_eq!(answer, "t", "a live backend was named");

    // The victim is told, then closed. No query needed — the signal wakes
    // it out of its blocking read.
    let msg = read_message(&mut victim).expect("the victim must be told");
    assert_eq!(msg.ty, b'E', "an ErrorResponse");
    assert_eq!(field_of(&msg, b'S').as_deref(), Some("FATAL"));
    assert_eq!(field_of(&msg, b'C').as_deref(), Some("57P01"));
    assert_eq!(
        field_of(&msg, b'M').as_deref(),
        Some("terminating connection due to administrator command"),
    );
    assert!(
        read_message(&mut victim).is_none(),
        "the connection must then be closed"
    );

    // And it is out of the registry, so a second attempt finds nothing.
    assert_eq!(
        scalar(
            &mut killer,
            &format!("SELECT pg_terminate_backend({victim_pid})")
        ),
        "f",
        "a terminated backend is no longer live"
    );
}

/// Cancelling a live but idle backend answers `t` and is a no-op — PG
/// clears an idle-time cancel rather than firing it at the next statement.
#[test]
fn cancelling_an_idle_backend_answers_true_and_spares_its_next_statement() {
    let (_guard, addr) = spawn();
    let mut killer = open(&addr, "killer");
    let mut victim = open(&addr, "victim");
    let victim_pid = scalar(&mut victim, "SELECT pg_backend_pid()");

    assert_eq!(
        scalar(
            &mut killer,
            &format!("SELECT pg_cancel_backend({victim_pid})")
        ),
        "t"
    );
    assert_eq!(
        scalar(&mut victim, "SELECT 1"),
        "1",
        "an idle-time cancel must not poison the next statement"
    );
}

/// V41 — `SET CONSTRAINTS` outside a transaction block succeeds but cannot
/// do anything, and PG says so with a WARNING. SPG used to succeed
/// silently; the notice channel could only speak NOTICE.
#[test]
fn set_constraints_outside_a_block_warns() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr, "alice");

    send_query(&mut s, "SET CONSTRAINTS ALL DEFERRED");
    let msgs = read_until_ready(&mut s);
    assert!(
        msgs.iter().any(|m| m.ty == b'C'),
        "the command still succeeds"
    );
    let notice = msgs
        .iter()
        .find(|m| m.ty == b'N')
        .expect("PG warns here");
    assert_eq!(field_of(notice, b'S').as_deref(), Some("WARNING"));
    assert_eq!(
        field_of(notice, b'M').as_deref(),
        Some("SET CONSTRAINTS can only be used in transaction blocks"),
    );

    // Inside a block it is legitimate — no warning.
    send_query(&mut s, "BEGIN");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "SET CONSTRAINTS ALL DEFERRED");
    let msgs = read_until_ready(&mut s);
    assert!(
        !msgs.iter().any(|m| m.ty == b'N'),
        "no warning inside a transaction block"
    );
    send_query(&mut s, "ROLLBACK");
    let _ = read_until_ready(&mut s);
}
