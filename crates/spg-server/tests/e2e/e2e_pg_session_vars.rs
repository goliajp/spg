//! v4.19 PG-wire SET / SHOW session variables.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-set-{nanos}"));
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

fn assert_cc_tag(msgs: &[PgMessage], expected: &str) {
    let cc = msgs.iter().find(|m| m.ty == b'C').unwrap_or_else(|| {
        panic!(
            "no CommandComplete in {:?}",
            msgs.iter().map(|m| m.ty as char).collect::<Vec<_>>()
        )
    });
    let tag = std::str::from_utf8(cc.body.strip_suffix(b"\0").unwrap_or(&cc.body)).unwrap();
    assert_eq!(tag, expected, "CC tag mismatch");
}

/// Extract column values from a single DataRow body (assumes
/// 1-row result). Returns the value of the first column as text.
fn first_cell(msgs: &[PgMessage]) -> String {
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    // ncols u16
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    assert!(len >= 0, "expected non-null cell, got len={len}");
    let start = 6;
    let end = start + len as usize;
    std::str::from_utf8(&dr.body[start..end])
        .unwrap()
        .to_string()
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = drain_until_ready(&mut s);
    s
}

#[test]
fn set_then_show_round_trips() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET application_name = 'myapp'");
    let msgs = drain_until_ready(&mut s);
    assert_cc_tag(&msgs, "SET");

    send_query(&mut s, "SHOW application_name");
    let msgs = drain_until_ready(&mut s);
    assert_eq!(first_cell(&msgs), "myapp");
}

#[test]
fn set_to_variant_also_works() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET search_path TO 'public,extra'");
    let msgs = drain_until_ready(&mut s);
    assert_cc_tag(&msgs, "SET");

    send_query(&mut s, "SHOW search_path");
    let msgs = drain_until_ready(&mut s);
    assert_eq!(first_cell(&msgs), "public,extra");
}

#[test]
fn show_returns_default_when_never_set() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SHOW client_encoding");
    let msgs = drain_until_ready(&mut s);
    assert_eq!(first_cell(&msgs), "UTF8");

    send_query(&mut s, "SHOW timezone");
    let msgs = drain_until_ready(&mut s);
    assert_eq!(first_cell(&msgs), "UTC");
}

#[test]
fn show_all_lists_known_settings() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SHOW ALL");
    let msgs = drain_until_ready(&mut s);
    let row_count = msgs.iter().filter(|m| m.ty == b'D').count();
    assert!(
        row_count >= 10,
        "SHOW ALL should list >=10 settings, got {row_count}"
    );
}

#[test]
fn set_session_local_prefix_accepted() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SET SESSION client_encoding = 'UTF8'");
    let msgs = drain_until_ready(&mut s);
    assert_cc_tag(&msgs, "SET");

    send_query(&mut s, "SET LOCAL statement_timeout = '0'");
    let msgs = drain_until_ready(&mut s);
    assert_cc_tag(&msgs, "SET");
}
