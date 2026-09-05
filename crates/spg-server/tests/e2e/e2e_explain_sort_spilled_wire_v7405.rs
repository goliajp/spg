//! v7.40.5 — `EXPLAIN ANALYZE` says which sort it just ran.
//!
//! `Sort Method` was hard-coded to `quicksort`, or `top-N heapsort`
//! under a LIMIT, and never asked whether the sort had gone to disk.
//! Differenced against PG 18.6 on this testbed, 200,000 rows of 64-byte
//! text at `work_mem = '4MB'`:
//!
//! ```text
//!   PG18.6   Sort Method: external merge  Disk: 13504kB
//!   SPG      Sort Method: quicksort
//! ```
//!
//! A DBA cannot tell a sort that fit from one that wrote thirteen
//! megabytes to disk, which is the question `work_mem` exists to answer.
//!
//! **This pin lives in the SERVER suite, and the first draft of it did
//! not.** An in-process `Engine` has no temp-run factory —
//! `ExternalSorter::push` says "spilling needs somewhere to spill to" and
//! runs unbounded without one — so an engine-level pin for this can only
//! ever see `quicksort`, and would have gone green against a fix that
//! did nothing. Same lesson as `e2e_sort_parallel_wire_v7404`: the thing
//! being pinned only exists on the path the server takes.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-explain-spill-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
}

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

fn rows(s: &mut TcpStream, sql: &str) -> Vec<String> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut got: Vec<String> = Vec::new();
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                assert!(n >= 1, "{sql}: a DataRow with no fields");
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                got.push(if len < 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned()
                });
            }
            b'E' => {
                let mut msg = String::new();
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
                    if tag == b'M' {
                        msg = String::from_utf8_lossy(&body[pos..end]).into_owned();
                    }
                    pos = end + 1;
                }
                panic!("{sql}: {msg}");
            }
            b'Z' => return got,
            _ => {}
        }
    }
}

fn seeded() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    rows(&mut s, "CREATE TABLE s (id int, t text)");
    // ~2.6 MB of text, which a 64 kB budget cannot hold and a 64 MB one
    // holds easily — the two sides this file asserts.
    rows(
        &mut s,
        "INSERT INTO s SELECT g, md5(g::text) || md5((g*7)::text) \
         FROM generate_series(1,40000) g",
    );
    (raw, s)
}

fn sort_method_lines(s: &mut TcpStream, sql: &str) -> Vec<String> {
    rows(s, sql)
        .into_iter()
        .filter(|l| l.contains("Sort Method"))
        .collect()
}

#[test]
fn a_sort_that_spilled_says_external_merge() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 64");
    let m = sort_method_lines(&mut s, "EXPLAIN ANALYZE SELECT t FROM s ORDER BY t");
    assert!(!m.is_empty(), "no Sort Method line at all");
    assert!(
        m.iter().any(|l| l.contains("external merge")),
        "a sort that spilled must say so, as PG's does: {m:?}"
    );
    // PG prints the volume beside the word, and a DBA sizing `work_mem`
    // reads the number rather than the word.
    assert!(
        m.iter().any(|l| l.contains("Disk:") && l.contains("kB")),
        "external merge carries a Disk figure in PG: {m:?}"
    );
}

/// The other direction, so the fix cannot be "always say external merge".
#[test]
fn a_sort_that_fit_still_says_quicksort() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 65536");
    let m = sort_method_lines(&mut s, "EXPLAIN ANALYZE SELECT t FROM s ORDER BY t");
    assert!(!m.is_empty(), "no Sort Method line at all");
    assert!(
        m.iter().any(|l| l.contains("quicksort")),
        "a sort that fit is not an external merge: {m:?}"
    );
}

/// And a plain EXPLAIN still carries no runtime fact, as PG's does not.
#[test]
fn a_plain_explain_still_names_no_method() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 64");
    let p = rows(&mut s, "EXPLAIN SELECT t FROM s ORDER BY t");
    assert!(
        !p.iter().any(|l| l.contains("Sort Method")),
        "a plain EXPLAIN has no Sort Method line in PG: {p:?}"
    );
}
