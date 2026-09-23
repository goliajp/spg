//! 9.1.0 — `spg_stat_query` sees statements sent through the extended
//! protocol.
//!
//! The per-statement timings and the slow-query log were recorded by
//! the simple-query path only. sqlx, asyncpg and most drivers send
//! EVERYTHING through Parse/Bind/Execute, so a view meant to answer
//! "which statement is slow" covered the protocol the application does
//! not use — and said so nowhere. An operator reading it saw a short,
//! plausible list.
//!
//! The text recorded is the Parse message's, not the bind-final render:
//! one entry per prepared statement, the way `pg_stat_statements`
//! groups them, and nothing is rendered on the hot path because the
//! server already holds it.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut h = [0u8; 5];
    s.read_exact(&mut h).expect("pg header");
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (h[0], body)
}

fn startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let mut out = Vec::new();
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    to_ready(s);
}

fn to_ready(s: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut seen = Vec::new();
    loop {
        let m = msg(s);
        let ty = m.0;
        seen.push(m);
        if ty == b'Z' {
            return seen;
        }
    }
}

fn simple(s: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let mut out = vec![b'Q'];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    to_ready(s)
}

/// Parse / Bind / Execute / Sync — what a driver sends.
fn extended(s: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    let mut p = Vec::new();
    p.push(0); // unnamed statement
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes()); // no declared param types
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);

    let mut b = Vec::new();
    b.push(0); // unnamed portal
    b.push(0); // unnamed statement
    b.extend_from_slice(&0u16.to_be_bytes()); // param formats
    b.extend_from_slice(&0u16.to_be_bytes()); // params
    b.extend_from_slice(&0u16.to_be_bytes()); // result formats
    out.push(b'B');
    out.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&b);

    let mut e = Vec::new();
    e.push(0); // unnamed portal
    e.extend_from_slice(&0u32.to_be_bytes()); // no row limit
    out.push(b'E');
    out.extend_from_slice(&((e.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&e);

    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();
    to_ready(s);
}

/// The rows a query answered — and a panic if it was REFUSED.
///
/// The first cut of this collected DataRow messages and nothing else,
/// so a query naming a column that does not exist read as "no rows".
/// It named `query` where the view's column is `sql`, and the test
/// reported the product as broken for two runs.
fn rows_text(seen: &[(u8, Vec<u8>)]) -> String {
    if let Some((_, b)) = seen.iter().find(|(t, _)| *t == b'E') {
        panic!("the query was refused: {}", String::from_utf8_lossy(b));
    }
    seen.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, b)| String::from_utf8_lossy(b).to_string())
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn a_statement_sent_through_parse_bind_execute_is_recorded() {
    let dir = unique_tmpdir("statq");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    startup(&mut s);
    simple(&mut s, "CREATE TABLE t (id int PRIMARY KEY)");

    // A statement no simple query ever sends, so finding it can only
    // mean the extended path recorded it.
    const MARKER: &str = "SELECT 424242 AS only_via_extended FROM t";
    extended(&mut s, MARKER);

    // The probe first: a simple query IS recorded, so an empty answer
    // below means the extended path was not, and not that the readback
    // is broken.
    // The whole table, and the control read from it: the view stores
    // the statement LOWERCASED and with its literals replaced by `$N`,
    // the way `pg_stat_statements` groups them — so a case-sensitive
    // `LIKE` finds nothing and reads exactly like a missing row.
    let all = rows_text(&simple(&mut s, "SELECT sql FROM spg_stat_query"));
    assert!(
        all.contains("create table t"),
        "the readback itself is broken: {all:?}"
    );
    let text = all;
    assert!(
        text.contains("only_via_extended"),
        "spg_stat_query did not see the extended-protocol statement; it holds {text:?}"
    );
}
