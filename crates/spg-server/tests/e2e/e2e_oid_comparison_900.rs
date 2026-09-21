//! 9.0.0 (N14) — `oid = numeric` answered `t` over the wire.
//!
//! PostgreSQL 18.6 refuses it: `operator does not exist: oid = numeric`,
//! and the same for `real` and `text`. `oid` shares its family with the
//! three integer widths and the reg types and with nothing else, which
//! is what round N12 measured and what the engine already enforced —
//! in process.
//!
//! Over the wire it answered `t`, because an autocommit SELECT takes a
//! read-only streaming route that runs BELOW the executor's gate and
//! carried its own COPY of the check list. There were three lists, and
//! the oid check had reached exactly one of them. They are one list now.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
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
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = vec![ty];
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).expect("write");
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608u32.to_be_bytes());
    for (k, v) in [("user", "postgres"), ("database", "probe")] {
        startup.extend_from_slice(k.as_bytes());
        startup.push(0);
        startup.extend_from_slice(v.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut framed = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&startup);
    s.write_all(&framed).unwrap();
    loop {
        let m = read_message(&mut s);
        if m.ty == b'Z' {
            break;
        }
    }
    s
}

fn query(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let done = m.ty == b'Z';
        out.push(m);
        if done {
            return out;
        }
    }
}

fn error_of(s: &mut TcpStream, sql: &str) -> Option<String> {
    let msgs = query(s, sql);
    let e = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < e.body.len() && e.body[i] != 0 {
        let code = e.body[i];
        let start = i + 1;
        let end = e.body[start..]
            .iter()
            .position(|&b| b == 0)
            .map_or(e.body.len(), |p| start + p);
        if code == b'M' {
            return Some(String::from_utf8_lossy(&e.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

#[test]
fn an_oid_compared_with_a_non_integer_is_refused_on_the_wire_too() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-oidcmp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    assert_eq!(
        error_of(
            &mut s,
            "CREATE TABLE n14t(o oid, n numeric, r real, i int, t text)"
        ),
        None
    );
    assert_eq!(
        error_of(&mut s, "INSERT INTO n14t VALUES (1,1,1,1,'1')"),
        None
    );

    // PostgreSQL 18.6's sentences, measured.
    for (sql, expected) in [
        (
            "SELECT o = n FROM n14t",
            "operator does not exist: oid = numeric",
        ),
        (
            "SELECT o = r FROM n14t",
            "operator does not exist: oid = real",
        ),
        (
            "SELECT o = t FROM n14t",
            "operator does not exist: oid = text",
        ),
        (
            "SELECT 1 FROM n14t WHERE o = n",
            "operator does not exist: oid = numeric",
        ),
    ] {
        assert_eq!(error_of(&mut s, sql).as_deref(), Some(expected), "{sql}");
    }

    // …and what PostgreSQL ALLOWS still answers: oid shares its family
    // with the integer widths.
    assert_eq!(error_of(&mut s, "SELECT o = i FROM n14t"), None);
    assert_eq!(error_of(&mut s, "SELECT o = 1 FROM n14t"), None);
    assert_eq!(error_of(&mut s, "SELECT o = 1::bigint FROM n14t"), None);
}
