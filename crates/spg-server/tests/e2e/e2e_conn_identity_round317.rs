//! read01 round 317 (V36) — a connection's id identifies THAT connection.
//!
//! `pg_backend_pid()` is what a CancelRequest, `pg_stat_activity.pid` and
//! every admin tool use to name one backend. pgwire used to derive it as
//! `process::id() + active_connections`, which repeats the moment one
//! connection leaves and another arrives: the counter falls back to a
//! value a still-live connection already took, so two live backends
//! answered the same pid and neither could be addressed on its own.
//!
//! Both wires now draw from one process-wide monotonic allocator.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-connid-{label}-{nanos}"));
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

/// Concurrently live connections must have distinct pids, and each must
/// see its own.
#[test]
fn live_pgwire_connections_have_distinct_pids() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let mut b = open(&addr, "bob");
    let mut c = open(&addr, "carol");

    let pa = scalar(&mut a, "SELECT pg_backend_pid()");
    let pb = scalar(&mut b, "SELECT pg_backend_pid()");
    let pc = scalar(&mut c, "SELECT pg_backend_pid()");
    assert_ne!(pa, pb, "alice and bob share a pid: {pa}");
    assert_ne!(pb, pc, "bob and carol share a pid: {pb}");
    assert_ne!(pa, pc, "alice and carol share a pid: {pa}");
    assert_eq!(
        scalar(&mut a, "SELECT pg_backend_pid()"),
        pa,
        "a connection's pid is stable"
    );
}

/// The failing shape of the old scheme: connect, disconnect, connect
/// again. The pid was derived from the LIVE connection count, so the
/// replacement connection was handed the pid of a connection that is
/// still attached.
#[test]
fn a_reconnect_does_not_reuse_a_live_connections_pid() {
    let (_guard, addr) = spawn();
    let mut keeper = open(&addr, "keeper");
    let keeper_pid = scalar(&mut keeper, "SELECT pg_backend_pid()");

    let mut churn = open(&addr, "churn");
    let churn_pid = scalar(&mut churn, "SELECT pg_backend_pid()");
    assert_ne!(keeper_pid, churn_pid);
    drop(churn);

    for i in 0..5 {
        let mut next = open(&addr, "next");
        let pid = scalar(&mut next, "SELECT pg_backend_pid()");
        assert_ne!(
            pid, keeper_pid,
            "round {i}: a new connection took the still-live keeper's pid"
        );
        drop(next);
    }
    assert_eq!(
        scalar(&mut keeper, "SELECT pg_backend_pid()"),
        keeper_pid,
        "the keeper kept its own pid throughout"
    );
}

/// `pg_stat_activity.pid` is the same number the connection reports for
/// itself — that self-join is how monitoring finds its own row.
#[test]
fn stat_activity_pid_matches_pg_backend_pid() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let pid = scalar(&mut a, "SELECT pg_backend_pid()");
    let found = scalar(
        &mut a,
        "SELECT pid FROM pg_stat_activity WHERE pid = pg_backend_pid()",
    );
    assert_eq!(found, pid);
}
