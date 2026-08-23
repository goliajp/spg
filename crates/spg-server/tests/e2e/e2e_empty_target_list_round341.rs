//! read01 round 341 (V66) — a zero-column result set over the wire.
//!
//! `SELECT FROM t` is legal PG and answers one **zero-column** row per
//! row of t. The engine gained that in this round; the wire has to carry
//! it too — a RowDescription with no fields, then one empty DataRow per
//! row, then `SELECT 3`. A client counting rows (psql prints `(3 rows)`)
//! reads exactly that.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-emptytl-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn a_zero_column_result_set_crosses_the_wire() {
    let db = unique_tmpdir("svc").join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.pgwire.expect("pgwire addr");
    let mut s = open(&addr);

    send_query(&mut s, "CREATE TABLE t (a INT)");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1),(2),(3)");
    let _ = read_until_ready(&mut s);

    send_query(&mut s, "SELECT FROM t");
    let msgs = read_until_ready(&mut s);

    let desc = msgs
        .iter()
        .find(|m| m.ty == b'T')
        .expect("a RowDescription, even with no fields");
    assert_eq!(
        u16::from_be_bytes([desc.body[0], desc.body[1]]),
        0,
        "field count"
    );

    let data: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert_eq!(data.len(), 3, "one DataRow per row of t");
    for d in data {
        assert_eq!(u16::from_be_bytes([d.body[0], d.body[1]]), 0, "cell count");
    }

    let done = msgs.iter().find(|m| m.ty == b'C').expect("CommandComplete");
    let tag = std::str::from_utf8(&done.body[..done.body.len() - 1]).unwrap();
    assert_eq!(tag, "SELECT 3");
}
