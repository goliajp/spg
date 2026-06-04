//! v6.5.3 — `spg_audit_chain` + `spg_audit_verify` virtual tables.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(
    db: &std::path::Path,
    audit: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .env("SPG_AUDIT", audit.to_string_lossy().into_owned())
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-audit-{label}-{nanos}"));
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

fn exec_simple(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let _ = read_until_ready(s);
}

#[allow(dead_code)]
fn count_rows(s: &mut TcpStream, sql: &str) -> i64 {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    let v = std::str::from_utf8(&dr.body[6..6 + len as usize]).unwrap();
    v.parse().unwrap()
}

#[test]
fn clean_chain_verifies() {
    let dir = unique_tmpdir("clean");
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");
    let (raw, addrs) = local_spawn(&db, &audit);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE t (id INT)");
    exec_simple(&mut s, "INSERT INTO t VALUES (1)");
    exec_simple(&mut s, "INSERT INTO t VALUES (2)");

    // spg_audit_verify returns (verified_count, broken_at_seq).
    send_query(&mut s, "SELECT * FROM spg_audit_verify");
    let msgs = read_until_ready(&mut s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    // 2 cells: verified_count, broken_at_seq.
    let cell_count = u16::from_be_bytes([dr.body[0], dr.body[1]]) as usize;
    assert_eq!(cell_count, 2);
    let len_a = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]) as usize;
    let verified = std::str::from_utf8(&dr.body[6..6 + len_a])
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let off_b = 6 + len_a;
    let len_b = i32::from_be_bytes([
        dr.body[off_b],
        dr.body[off_b + 1],
        dr.body[off_b + 2],
        dr.body[off_b + 3],
    ]) as usize;
    let broken = std::str::from_utf8(&dr.body[off_b + 4..off_b + 4 + len_b])
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert!(
        verified >= 3,
        "≥3 audit entries (1 CREATE + 2 INSERT), got {verified}"
    );
    assert_eq!(broken, -1, "clean chain → broken_at_seq = -1");
}

#[test]
fn chain_table_lists_entries() {
    let dir = unique_tmpdir("chain-list");
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");
    let (raw, addrs) = local_spawn(&db, &audit);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE t (id INT)");
    exec_simple(&mut s, "INSERT INTO t VALUES (1)");
    exec_simple(&mut s, "INSERT INTO t VALUES (2)");

    // Virtual-table short-circuit only fires for `SELECT * FROM …`,
    // so count rows client-side rather than `SELECT count(*)`.
    send_query(&mut s, "SELECT * FROM spg_audit_chain");
    let msgs = read_until_ready(&mut s);
    let data_rows: usize = msgs.iter().filter(|m| m.ty == b'D').count();
    assert!(data_rows >= 3, "expected ≥3 audit rows, got {data_rows}");
}

#[test]
fn empty_log_verifies_with_zero_count() {
    let dir = unique_tmpdir("empty");
    let db = dir.join("spg.db");
    let audit = dir.join("audit.log");
    let (raw, addrs) = local_spawn(&db, &audit);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // No DML / DDL yet — audit log is empty.
    send_query(&mut s, "SELECT * FROM spg_audit_verify");
    let msgs = read_until_ready(&mut s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]) as usize;
    let verified = std::str::from_utf8(&dr.body[6..6 + len])
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert_eq!(verified, 0, "no DML/DDL → 0 verified entries");
}
