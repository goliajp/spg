//! r791 — the wire contract for a SELECT that fails after rows exist.
//!
//! A SCALARSQ-shape SELECT (single table, scalar subquery in the
//! projection) streams straight into the write buffer, so a runtime
//! error on row N arrives with rows 1..N-1 already encoded. Two error
//! classes are covered here: a per-row evaluation failure (division by
//! zero, SQLSTATE 22012 — same as PG 18.4) and the query byte budget.
//!
//! What these pin: one ErrorResponse, no CommandComplete, and no
//! DataRow beyond the prefix that was produced before the failure.
//!
//! What they do NOT pin: that the statement executes only once. Until
//! r791 the `!wrote_header` arm in `handle_pg_simple_query_one_into_wbuf`
//! rewound the buffer and re-ran the whole statement through
//! `execute_with_role`, because the SCALARSQ branch set `wrote_header`
//! after the executor returned rather than when the RowDescription was
//! written. Both runs reported the same error, so the wire bytes were
//! identical and no assertion here could have caught it — it was found
//! by instrumenting the two error arms and reading the counts (1741
//! bytes of encoded rows discarded, then a second execution). SPG has
//! no per-statement execution counter to assert against, and adding one
//! for a test is not worth the surface.
//!
//! The DataRow assertion becomes load-bearing once the write buffer
//! flushes incrementally: already-flushed rows cannot be withdrawn, so
//! a re-execution would put the prefix on the wire twice (198 rows for
//! a 200-row table failing at row 100, never more than 99).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 5);
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn exec(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-r791-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// SQLSTATE out of an ErrorResponse body — field 'C' in the
/// NUL-separated code/value list.
fn sqlstate_of(m: &PgMessage) -> String {
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0).unwrap();
        if code == b'C' {
            return String::from_utf8_lossy(&m.body[start..end]).into_owned();
        }
        i = end + 1;
    }
    String::new()
}

fn seed(s: &mut TcpStream, wide: bool) {
    let val = if wide { "repeat('x', 200)" } else { "g::TEXT" };
    exec(s, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)");
    exec(
        s,
        &format!("INSERT INTO t SELECT g, {val} FROM generate_series(1,200) g"),
    );
    exec(s, "CREATE TABLE probe (k INT PRIMARY KEY, n INT)");
    exec(s, "INSERT INTO probe VALUES (1,7)");
}

#[test]
fn midstream_division_by_zero_reports_once() {
    let dir = unique_tmpdir("divzero");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, false);

    let msgs = exec(
        &mut s,
        "SELECT (SELECT p.n FROM probe p WHERE p.k = 1) AS a, 100/(100 - t.id) AS c FROM t",
    );

    let errors: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'E').collect();
    assert_eq!(errors.len(), 1, "exactly one ErrorResponse");
    assert_eq!(
        sqlstate_of(errors[0]),
        "22012",
        "division by zero, as PG 18.4 reports it"
    );
    assert!(
        !msgs.iter().any(|m| m.ty == b'C'),
        "a failed SELECT owes no CommandComplete"
    );
    let rows = msgs.iter().filter(|m| m.ty == b'D').count();
    assert!(
        rows < 100,
        "at most the 99-row prefix before row 100 divides by zero, got {rows}"
    );
}

#[test]
fn midstream_byte_budget_reports_once() {
    let dir = unique_tmpdir("budget");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .env("SPG_MAX_QUERY_BYTES", "4096")
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, true);

    let msgs = exec(
        &mut s,
        "SELECT (SELECT p.n FROM probe p WHERE p.k = 1) AS a, t.v FROM t",
    );

    let errors: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'E').collect();
    assert_eq!(
        errors.len(),
        1,
        "the budget stops the query once, not once per path"
    );
    assert!(
        !msgs.iter().any(|m| m.ty == b'C'),
        "a budget-stopped SELECT owes no CommandComplete"
    );
}
