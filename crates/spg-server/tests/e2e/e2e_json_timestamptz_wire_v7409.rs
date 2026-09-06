//! v7.40.9 — the session zone half of the `timestamptz`-in-JSON defect.
//!
//! The offset itself is pinned in the engine suite
//! (`e2e_json_timestamptz_v7409`). The ZONE cannot be: probed on an
//! in-process `Engine` with `SET TimeZone = 'Asia/Tokyo'` in force and
//! `SHOW TimeZone` answering `Asia/Tokyo`, `ts::text` still renders
//! `2026-01-01 00:00:00+00`. The session zone does not reach evaluation
//! there at all, which predates this change and is not what this fix
//! owns — an assertion there would pass for the wrong reason.
//!
//! Over the wire it does reach it. Measured against the published
//! 7.40.7 image before the fix:
//!
//! ```text
//!   SET TimeZone = 'Asia/Tokyo'
//!     to_jsonb(ts)     "2026-01-01T09:00:00+09:00"   correct already
//!     row_to_json(t)   "2026-01-01T00:00:00"         no zone, no offset
//! ```
//!
//! and against PG 18.6 on the same row, where all four forms answer
//! `"2026-01-01T09:00:00+09:00"`.

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
    let p = common::tmp_base().join(format!("spg-json-tz-{nanos}"));
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
    rows(&mut s, "CREATE TABLE tzr (id int, ts timestamptz)");
    rows(
        &mut s,
        "INSERT INTO tzr VALUES (1, timestamptz '2026-01-01 00:00:00+00')",
    );
    (raw, s)
}

/// Every builder, in the session's zone. The scalar was right before
/// this fix and is here as the reference the others must agree with.
#[test]
fn every_json_builder_spells_the_instant_in_the_session_zone() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET TimeZone = 'Asia/Tokyo'");

    let want = "2026-01-01T09:00:00+09:00";
    for sql in [
        "SELECT to_jsonb(ts)::text FROM tzr",
        "SELECT row_to_json(t)::text FROM tzr t",
        "SELECT to_jsonb(t)::text FROM tzr t",
        "SELECT json_build_object('ts', ts)::text FROM tzr",
    ] {
        let got = rows(&mut s, sql);
        assert!(
            got.iter().any(|r| r.contains(want)),
            "{sql}: expected {want}, got {got:?}"
        );
    }
}

/// And in a different zone, so the fix cannot be a constant.
#[test]
fn a_second_zone_gives_a_second_answer() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET TimeZone = 'UTC'");
    let got = rows(&mut s, "SELECT row_to_json(t)::text FROM tzr t");
    assert!(
        got.iter().any(|r| r.contains("2026-01-01T00:00:00+00:00")),
        "under UTC: {got:?}"
    );
    assert!(
        !got.iter().any(|r| r.contains("+09:00")),
        "the Tokyo answer must not survive a zone change: {got:?}"
    );
}

/// The customer's own statement: their CLI dumps every table with it.
#[test]
fn the_dump_statement_carries_the_zone() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET TimeZone = 'UTC'");
    let got = rows(&mut s, "SELECT row_to_json(t) FROM tzr t");
    assert_eq!(
        got,
        vec!["{\"id\":1,\"ts\":\"2026-01-01T00:00:00+00:00\"}".to_string()],
        "a dump taken from SPG must carry what one taken from PostgreSQL does"
    );
}

/// v7.40.9 — `generate_series(...)` in FROM described NO columns, so a
/// driver using the extended protocol got a protocol error rather than
/// an answer.
///
/// Found by running a customer's own repro through a real Bind against
/// the published 7.40.8 image, after shipping the fix their report
/// asked for:
///
/// ```text
///   SELECT count(*) FROM generate_series(1,5) g
///     simple query        5
///     extended protocol   server sent data ("D" message) without prior
///                         row description ("T" message)
/// ```
///
/// Present on 7.39.0, 7.40.7 and 7.40.8, with and without bound
/// parameters — so it is the SHAPE, not the parameter. `unnest(...)`
/// described fine because v7.38.3 taught `describe` about that slot and
/// `generate_series` has its own, which is the same pair of fields the
/// parameter-substitution walk had to learn about on the same day.
#[test]
fn generate_series_in_from_describes_its_column() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    // The simple-query protocol always answered; the extended one is
    // what a driver uses, and it is the one that broke.
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM generate_series(1,5) g"),
        vec!["5"]
    );
    assert_eq!(
        extended(&mut s, "SELECT count(*) FROM generate_series(1,5) g", &[]),
        vec!["5"],
        "a driver that prepares this statement must get the same answer"
    );
    assert_eq!(
        extended(
            &mut s,
            "SELECT count(*) FROM generate_series($1::int, $2::int) g",
            &["1", "5"]
        ),
        vec!["5"],
        "and with the bounds bound"
    );
    assert_eq!(
        extended(
            &mut s,
            "SELECT count(*) FROM unnest($1::int[]) AS u(x)",
            &["{1,2,3}"]
        ),
        vec!["3"],
        "the sibling slot, which has worked since v7.38.3"
    );
}

/// One extended-protocol round trip: Parse, Bind (text parameters),
/// Describe the portal, Execute, Sync. The Describe is the point — it
/// is what produces the RowDescription the client insists on seeing
/// before any DataRow.
fn extended(s: &mut TcpStream, sql: &str, params: &[&str]) -> Vec<String> {
    let mut out: Vec<u8> = Vec::new();
    // Parse (unnamed statement, no declared parameter types)
    let mut p: Vec<u8> = Vec::new();
    p.push(0);
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);
    // Bind (unnamed portal, all text in, all text out)
    let mut b: Vec<u8> = Vec::new();
    b.push(0);
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(u16::try_from(params.len()).unwrap()).to_be_bytes());
    for v in params {
        b.extend_from_slice(&(i32::try_from(v.len()).unwrap()).to_be_bytes());
        b.extend_from_slice(v.as_bytes());
    }
    b.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'B');
    out.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&b);
    // Describe the portal
    let d: Vec<u8> = vec![b'P', 0];
    out.push(b'D');
    out.extend_from_slice(&((d.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&d);
    // Execute, unlimited
    let mut e: Vec<u8> = Vec::new();
    e.push(0);
    e.extend_from_slice(&0u32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((e.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&e);
    // Sync
    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();

    let mut saw_row_description = false;
    let mut got: Vec<String> = Vec::new();
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'T' => saw_row_description = true,
            b'D' => {
                assert!(
                    saw_row_description,
                    "{sql}: a DataRow before any RowDescription — this is the defect"
                );
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
