//! v6.5.2 — `spg_stat_activity` virtual table over pgwire.
//!
//! Active pgwire connections register themselves in the server-side
//! registry; the virtual table reads through the engine's
//! activity_provider callback.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-activity-{label}-{nanos}"));
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

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    let len = (body.len() + 4) as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    write_msg(&mut out, b'Q', &body);
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

fn open(addr: &str, user: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn open_connection_appears_in_activity() {
    let dir = unique_tmpdir("one-conn");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "alice");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    // Count DataRow frames.
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    // At minimum the connection running this query is in the registry.
    assert!(
        !data_rows.is_empty(),
        "expected at least one row for the open connection"
    );
}

#[test]
fn two_open_connections_each_have_a_row() {
    let dir = unique_tmpdir("two-conn");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);

    let _other = open(addrs.pgwire.as_ref().unwrap(), "bob");
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "alice");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert!(
        data_rows.len() >= 2,
        "expected >= 2 rows for two open connections, got {}",
        data_rows.len()
    );
}

#[test]
fn columns_match_design() {
    let dir = unique_tmpdir("cols");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    // Body: [u16 cell_count] [name\0 ...] per column.
    let cell_count = u16::from_be_bytes([rd.body[0], rd.body[1]]) as usize;
    assert_eq!(cell_count, 8, "spg_stat_activity has 8 columns");
}
