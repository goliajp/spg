//! v4.15 pgbouncer compat — verifies the connection-reset
//! statements pgbouncer issues between pooled client sessions
//! all return clean CommandComplete frames.

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

const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-pgb-{nanos}"));
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
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
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
        if m.ty == b'Z' {
            out.push(m);
            return out;
        }
        out.push(m);
    }
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn assert_cc_tag(msgs: &[PgMessage], expected_tag: &str) {
    let cc = msgs.iter().find(|m| m.ty == b'C').unwrap_or_else(|| {
        panic!(
            "no CommandComplete in {:?}",
            msgs.iter().map(|m| m.ty as char).collect::<Vec<_>>()
        )
    });
    // CC body is the null-terminated tag string.
    let tag = std::str::from_utf8(cc.body.strip_suffix(b"\0").unwrap_or(&cc.body)).unwrap();
    assert_eq!(tag, expected_tag, "CC tag mismatch");
}

#[test]
fn discard_all_returns_clean_cc() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    send_query(&mut s, "DISCARD ALL");
    let msgs = read_until_ready(&mut s);
    assert_cc_tag(&msgs, "DISCARD ALL");
}

#[test]
fn discard_temp_sequences_plans_each_work() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    for variant in ["DISCARD TEMP", "DISCARD SEQUENCES", "DISCARD PLANS"] {
        send_query(&mut s, variant);
        let msgs = read_until_ready(&mut s);
        // v7.39 (round 320, V53) — PG tags each subform with the target
        // it named (`DISCARD TEMP`, not a bare `DISCARD`); measured on
        // PG 18.4. The bare tag was the canned short-circuit's invention.
        assert_cc_tag(&msgs, variant);
    }
}

#[test]
fn reset_all_returns_cc() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    send_query(&mut s, "RESET ALL");
    let msgs = read_until_ready(&mut s);
    assert_cc_tag(&msgs, "RESET");
}

#[test]
fn set_transaction_isolation_returns_cc() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    send_query(&mut s, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    let msgs = read_until_ready(&mut s);
    assert_cc_tag(&msgs, "SET");
}
