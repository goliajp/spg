#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]

//! v4.17 PG-wire COPY FROM STDIN / COPY TO STDOUT.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

mod common;

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
    let p = std::env::temp_dir().join(format!("spg-e2e-copy-{nanos}"));
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn exec_simple(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let cc = msgs.iter().find(|m| m.ty == b'C');
    assert!(cc.is_some(), "no CommandComplete for {sql:?}");
}

#[test]
fn copy_from_stdin_inserts_rows() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(
        &mut s,
        "CREATE TABLE imp (id INT NOT NULL, name TEXT NOT NULL)",
    );

    // Begin COPY FROM STDIN.
    send_query(&mut s, "COPY imp FROM STDIN");
    let g = read_message(&mut s);
    assert_eq!(g.ty, b'G', "expected CopyInResponse");

    // Stream three rows.
    let payload = "1\talice\n2\tbob\n3\tcarol\n";
    send_msg(&mut s, b'd', payload.as_bytes());
    send_msg(&mut s, b'c', &[]); // CopyDone
    let msgs = read_until_ready(&mut s);
    let cc = msgs.iter().find(|m| m.ty == b'C').expect("CC");
    let tag = std::str::from_utf8(cc.body.strip_suffix(b"\0").unwrap_or(&cc.body)).unwrap();
    assert_eq!(tag, "COPY 3");

    // Verify rows landed.
    send_query(&mut s, "SELECT count(*) FROM imp");
    let msgs = read_until_ready(&mut s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    let v = std::str::from_utf8(&dr.body[6..6 + len as usize]).unwrap();
    assert_eq!(v, "3");
}

#[test]
fn copy_to_stdout_streams_rows() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(
        &mut s,
        "CREATE TABLE exp (id INT NOT NULL, name TEXT NOT NULL)",
    );
    exec_simple(&mut s, "INSERT INTO exp VALUES (1, 'alice')");
    exec_simple(&mut s, "INSERT INTO exp VALUES (2, 'bob')");

    send_query(&mut s, "COPY exp TO STDOUT");
    // Server replies: CopyOutResponse (H) + CopyData per row + CopyDone + CC + ReadyForQuery.
    let h = read_message(&mut s);
    assert_eq!(h.ty, b'H', "expected CopyOutResponse");

    let mut bytes = Vec::new();
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'd' => bytes.extend_from_slice(&m.body),
            b'c' => break,
            other => panic!("unexpected {} during copy out", other as char),
        }
    }
    // Drain CommandComplete + ReadyForQuery.
    let _ = read_until_ready(&mut s);

    let text = String::from_utf8(bytes).unwrap();
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines, vec!["1\talice", "2\tbob"]);
}

#[test]
fn copy_from_stdin_handles_null_marker() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE n (id INT NOT NULL, label TEXT)");
    send_query(&mut s, "COPY n FROM STDIN");
    let _ = read_message(&mut s); // G

    // Second row has NULL in the label column.
    let payload = "1\tx\n2\t\\N\n";
    send_msg(&mut s, b'd', payload.as_bytes());
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);

    // Verify via SELECT label FROM n WHERE id=2 — should be NULL.
    send_query(&mut s, "SELECT label FROM n WHERE id = 2");
    let msgs = read_until_ready(&mut s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    // i32 len = -1 means NULL
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    assert_eq!(
        len, -1,
        "expected NULL marker (-1) for label, got len={len}"
    );
}
