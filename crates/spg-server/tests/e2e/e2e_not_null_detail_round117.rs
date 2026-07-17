//! v7.39 (read01 round 117) — NOT NULL violation wire fidelity. PG's 23502
//! ErrorResponse carries the primary message in `M`, the SQLSTATE `23502` in
//! `C`, and `Failing row contains (...)` in its own `D` (detail) field — no
//! internal `unsupported:` class prefix, and the DETAIL is NOT inline in `M`.
//! Verified over the real pgwire protocol (the embedded engine builds the
//! message with an inline ` DETAIL: ` that the wire splits into `D`).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-nndetail-{label}-{nanos}"));
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
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
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
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
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

fn field(body: &[u8], code: u8) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let c = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if c == code {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        p = end + 1;
    }
    None
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn run_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "unexpected error for {sql}"
    );
}

#[test]
fn not_null_violation_splits_detail_field() {
    let dir = unique_tmpdir("split");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_ok(
        &mut s,
        "CREATE TABLE nn (a text, b int NOT NULL, c text DEFAULT 'x')",
    );

    send_query(&mut s, "INSERT INTO nn (a) VALUES ('hi')");
    let msgs = read_until_ready(&mut s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("no error"));

    // SQLSTATE 23502 (not_null_violation).
    assert_eq!(field(&e.body, b'C').as_deref(), Some("23502"));
    // Primary message: PG's exact wording, no internal class prefix, no inline
    // DETAIL.
    assert_eq!(
        field(&e.body, b'M').as_deref(),
        Some("null value in column \"b\" of relation \"nn\" violates not-null constraint")
    );
    // DETAIL rides its own `D` field, naming the fully-assembled failing row.
    assert_eq!(
        field(&e.body, b'D').as_deref(),
        Some("Failing row contains (hi, null, x).")
    );
}
