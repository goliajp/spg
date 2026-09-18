//! 9.0.0 — the type a column is ANNOUNCED as, and the bytes sent for it
//! in binary, for the types SPG described as `text` or could not encode.
//!
//! A driver decodes by the announced oid, and most of them ask for binary
//! results. Measured on PostgreSQL 18.6 with a raw extended-protocol
//! probe (result format 1):
//!
//! ```text
//!                         announced          binary payload
//!   pg_typeof(1)          2206 regtype       00000017
//!   'int4'::regtype       2206 regtype       00000017
//!   'pg_class'::regclass  2205 regclass      000004eb
//!   ctid                    27 tid           000000000001
//!   xmin                    28 xid           4 bytes
//!   cmin                    29 cid           4 bytes
//!   tableoid                26 oid           4 bytes
//!   point(1,2)             600 point
//!   box(…)                 603 box
//!   int4range(1,3)        3904 int4range
//!   inet_client_addr()     869 inet
//!   similarity('a','b')    700 real          (was float8, 701)
//! ```
//!
//! SPG 8.0.4 announced every one of these as `text` (25), and for the
//! first seven a binary request was refused outright with
//! `binary result format not implemented`.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

struct Msg {
    ty: u8,
    body: Vec<u8>,
}

fn read_msg(s: &mut TcpStream) -> Msg {
    let mut h = [0u8; 5];
    s.read_exact(&mut h).expect("header");
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    Msg { ty: h[0], body }
}

fn send(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = vec![ty];
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn until_ready(s: &mut TcpStream) -> Vec<Msg> {
    let mut out = Vec::new();
    loop {
        let m = read_msg(s);
        let done = m.ty == b'Z';
        out.push(m);
        if done {
            return out;
        }
    }
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0admin\0\0");
    let mut out = u32::try_from(body.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    until_ready(&mut s);
    s
}

fn simple(s: &mut TcpStream, sql: &str) {
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    send(s, b'Q', &b);
    let msgs = until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
}

/// `(announced oid, binary payload)` for each column of the first row.
fn binary(s: &mut TcpStream, sql: &str) -> Vec<(u32, Option<Vec<u8>>)> {
    let mut parse = vec![0u8];
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&0u16.to_be_bytes());
    send(s, b'P', &parse);
    // unnamed portal, unnamed statement, no params, ONE result format: binary
    let mut bind = vec![0u8, 0u8];
    bind.extend_from_slice(&0u16.to_be_bytes());
    bind.extend_from_slice(&0u16.to_be_bytes());
    bind.extend_from_slice(&1u16.to_be_bytes());
    bind.extend_from_slice(&1u16.to_be_bytes());
    send(s, b'B', &bind);
    send(s, b'D', b"P\0");
    let mut exec = vec![0u8];
    exec.extend_from_slice(&0u32.to_be_bytes());
    send(s, b'E', &exec);
    send(s, b'S', &[]);
    let msgs = until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
    let desc = msgs
        .iter()
        .find(|m| m.ty == b'T')
        .expect("a RowDescription");
    let n = u16::from_be_bytes([desc.body[0], desc.body[1]]) as usize;
    let mut oids = Vec::new();
    let mut p = 2;
    for _ in 0..n {
        let end = p + desc.body[p..].iter().position(|&b| b == 0).unwrap();
        p = end + 1;
        oids.push(u32::from_be_bytes(
            desc.body[p + 6..p + 10].try_into().unwrap(),
        ));
        p += 18;
    }
    let row = msgs.iter().find(|m| m.ty == b'D').expect("a DataRow");
    let mut out = Vec::new();
    let mut p = 2;
    for oid in oids {
        let len = i32::from_be_bytes(row.body[p..p + 4].try_into().unwrap());
        p += 4;
        let val = (len >= 0).then(|| row.body[p..p + len as usize].to_vec());
        p += len.max(0) as usize;
        out.push((oid, val));
    }
    out
}

#[test]
fn reference_and_system_types_are_announced_and_encoded_as_pg_does() {
    let dir = common::tmp_base().join(format!("spg-e2e-decltypes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    simple(&mut s, "CREATE TABLE bt(a int)");
    simple(&mut s, "INSERT INTO bt VALUES (1)");

    let r = binary(
        &mut s,
        "SELECT pg_typeof(1), 'int4'::regtype, 'pg_class'::regclass",
    );
    // 23 is int4's oid; 1259 is pg_class's, fixed in every PostgreSQL.
    assert_eq!(r[0], (2206, Some(23u32.to_be_bytes().to_vec())));
    assert_eq!(r[1], (2206, Some(23u32.to_be_bytes().to_vec())));
    assert_eq!(r[2], (2205, Some(1259u32.to_be_bytes().to_vec())));

    let r = binary(&mut s, "SELECT ctid, xmin, cmin, tableoid FROM bt");
    assert_eq!(r[0], (27, Some(vec![0, 0, 0, 0, 0, 1])), "ctid (0,1)");
    for (i, (want_oid, name)) in [(28, "xmin"), (29, "cmin"), (26, "tableoid")]
        .into_iter()
        .enumerate()
    {
        let (oid, val) = &r[i + 1];
        assert_eq!(*oid, want_oid, "{name}");
        assert_eq!(val.as_ref().map(Vec::len), Some(4), "{name} is four bytes");
    }

    // Announced types alone for the rest. pg_trgm's `similarity` exists
    // once the extension does, and is `real` (700), not float8.
    simple(&mut s, "CREATE EXTENSION pg_trgm");
    assert_eq!(describe_oid(&mut s, "SELECT similarity('abc','abd')"), 700);
    let oids: Vec<u32> = [
        "SELECT point(1,2)",
        "SELECT box(point(0,0),point(1,1))",
        "SELECT int4range(1,3)",
        "SELECT inet_client_addr()",
    ]
    .into_iter()
    .map(|q| describe_oid(&mut s, q))
    .collect();
    assert_eq!(oids, vec![600, 603, 3904, 869]);
}

/// The announced oid of a one-column statement, via Describe alone.
fn describe_oid(s: &mut TcpStream, sql: &str) -> u32 {
    let mut parse = vec![0u8];
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&0u16.to_be_bytes());
    send(s, b'P', &parse);
    send(s, b'D', b"S\0");
    send(s, b'S', &[]);
    let msgs = until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
    let desc = msgs
        .iter()
        .find(|m| m.ty == b'T')
        .expect("a RowDescription");
    let end = 2 + desc.body[2..].iter().position(|&b| b == 0).unwrap();
    let p = end + 1;
    u32::from_be_bytes(desc.body[p + 6..p + 10].try_into().unwrap())
}
