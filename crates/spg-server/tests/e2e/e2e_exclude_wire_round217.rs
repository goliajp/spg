//! v7.39 (round 217) — EXCLUDE constraint violations on the pgwire, verified
//! end-to-end (the embedded pins in r210 checked the engine message; this
//! pins the WIRE encoding ORMs actually branch on). Live-PG18.4 differential
//! (2026-07-18): an overlap raises
//!   SQLSTATE 23P01
//!   M: conflicting key value violates exclusion constraint "ov_during_excl"
//!   D: Key (during)=([3,7)) conflicts with existing key (during)=([1,5)).
//! PG's message carries NO ` on table "…"` suffix — the wire layer strips the
//! engine-side suffix, and lifts the constraint name into PG_DIAG 'n'. This
//! test asserts all four (C/M/D/n) byte-for-byte.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-excl-wire-{nanos}"));
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

/// All ErrorResponse fields of the first 'E' the query raises, keyed by their
/// tag byte (C=SQLSTATE, M=message, D=detail, n=constraint name, …). `None`
/// on success.
fn exec_error_fields(
    s: &mut TcpStream,
    sql: &str,
) -> Option<std::collections::HashMap<u8, String>> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut fields: Option<std::collections::HashMap<u8, String>> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'E' => {
                let mut map = std::collections::HashMap::new();
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
                    map.insert(tag, String::from_utf8_lossy(&body[pos..end]).into_owned());
                    pos = end + 1;
                }
                fields = Some(map);
            }
            b'Z' => return fields,
            _ => {}
        }
    }
}

/// Run a single-row / single-column SELECT and return that value's text
/// (the first DataRow's first column), or `None` if the column was SQL NULL.
fn exec_first_value(s: &mut TcpStream, sql: &str) -> Option<String> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut value: Option<String> = None;
    let mut have_row = false;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' if !have_row => {
                have_row = true;
                // DataRow: [i16 ncols][ per col: i32 len (-1 = NULL) + bytes ].
                let ncols = i16::from_be_bytes([body[0], body[1]]);
                if ncols >= 1 {
                    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                    if len >= 0 {
                        let start = 6usize;
                        let end = start + len as usize;
                        value = Some(String::from_utf8_lossy(&body[start..end]).into_owned());
                    }
                }
            }
            b'Z' => return value,
            _ => {}
        }
    }
}

#[test]
fn exclude_catalog_reflection_over_wire() {
    // The pg_dump path: pg_constraint contype + pg_get_constraintdef travel
    // the wire as ordinary DataRows. Verify the deparse a dumper reads.
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(
        exec_error_fields(
            &mut s,
            "CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))"
        ),
        None
    );
    assert_eq!(
        exec_first_value(
            &mut s,
            "SELECT contype FROM pg_constraint WHERE contype = 'x'"
        )
        .as_deref(),
        Some("x")
    );
    assert_eq!(
        exec_first_value(
            &mut s,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'ov_during_excl'"
        )
        .as_deref(),
        Some("EXCLUDE USING gist (during WITH &&)")
    );
}

#[test]
fn exclusion_violation_wire_fields_match_pg() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());

    assert_eq!(
        exec_error_fields(
            &mut s,
            "CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))"
        ),
        None
    );
    assert_eq!(
        exec_error_fields(&mut s, "INSERT INTO ov VALUES ('[1,5)')"),
        None
    );
    // [3,7) overlaps [1,5) → 23P01.
    let f = exec_error_fields(&mut s, "INSERT INTO ov VALUES ('[3,7)')")
        .expect("overlap must raise an ErrorResponse");
    assert_eq!(f.get(&b'C').map(String::as_str), Some("23P01"), "SQLSTATE");
    assert_eq!(
        f.get(&b'M').map(String::as_str),
        Some("conflicting key value violates exclusion constraint \"ov_during_excl\""),
        "message must match PG (no ` on table` suffix)"
    );
    assert_eq!(
        f.get(&b'D').map(String::as_str),
        Some("Key (during)=([3,7)) conflicts with existing key (during)=([1,5))."),
        "DETAIL"
    );
    // PG_DIAG 'n' (constraint name) — ORMs regex it out.
    assert_eq!(
        f.get(&b'n').map(String::as_str),
        Some("ov_during_excl"),
        "constraint-name diag field"
    );
}
