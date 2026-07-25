//! read01 round 476 (C4) — `pg_current_wal_lsn()` reports the WAL's real
//! byte position.
//!
//! It answered the literal `0/0` forever, so every monitor watching WAL
//! progress saw an instance that had never written anything, and
//! `pg_wal_lsn_diff` over two samples was always zero. SPG's WAL is a file
//! and its length IS an LSN in every sense a monitor uses one: monotonic,
//! byte-denominated, and comparable.
//!
//! The first cut counted bytes in `append_wal` only. There are three append
//! paths, and the LSN froze the moment traffic took the group-commit route —
//! which is the route the write panel uses. Reading the file's length cannot
//! be forgotten by a new path, so that is what it does. This pin compares
//! the reported LSN against the file on disk.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut ty = [0u8; 1];
    s.read_exact(&mut ty).unwrap();
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let n = i32::from_be_bytes(len) as usize - 4;
    let mut body = vec![0u8; n];
    s.read_exact(&mut body).unwrap();
    (ty[0], body)
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut out = vec![b'Q'];
    let body_len = sql.len() + 1 + 4;
    out.extend_from_slice(&i32::try_from(body_len).unwrap().to_be_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    s.write_all(&out).unwrap();
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(user.as_bytes());
    params.push(0);
    params.extend_from_slice(b"database\0bench\0");
    params.push(0);
    let mut out = Vec::new();
    out.extend_from_slice(&i32::try_from(params.len() + 8).unwrap().to_be_bytes());
    out.extend_from_slice(&196_608i32.to_be_bytes());
    out.extend_from_slice(&params);
    s.write_all(&out).unwrap();
    loop {
        let (ty, _) = pg_msg(s);
        if ty == b'Z' {
            return;
        }
    }
}

fn first_cell(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let mut cell = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                cell = Some(String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned());
            }
            b'Z' => return cell.expect("no DataRow"),
            _ => {}
        }
    }
}

fn run(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    loop {
        if pg_msg(s).0 == b'Z' {
            return;
        }
    }
}

/// `X/Y` back to a byte count.
fn lsn_bytes(text: &str) -> u64 {
    let (hi, lo) = text.split_once('/').expect("LSN is X/Y");
    (u64::from_str_radix(hi, 16).unwrap() << 32) | u64::from_str_radix(lo, 16).unwrap()
}

#[test]
fn round476_wal_lsn_is_the_files_real_byte_position() {
    let dir = std::env::temp_dir().join(format!(
        "spg-e2e-wal-lsn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("wal.log");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&wal)
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(addrs.pgwire.as_ref().unwrap());
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "bench");

    run(&mut s, "CREATE TABLE t (a INT)");
    run(&mut s, "INSERT INTO t VALUES (1),(2),(3)");
    let first = lsn_bytes(&first_cell(&mut s, "SELECT pg_current_wal_lsn()"));
    assert!(first > 0, "a WAL with writes in it is not at 0/0");
    assert_eq!(
        first,
        std::fs::metadata(&wal).unwrap().len(),
        "the LSN must be the file's byte length"
    );

    // The route the first cut missed: more traffic must move it.
    run(&mut s, "INSERT INTO t VALUES (4)");
    let second = lsn_bytes(&first_cell(&mut s, "SELECT pg_current_wal_lsn()"));
    assert!(
        second > first,
        "the LSN must advance with writes: {first} -> {second}"
    );
    assert_eq!(second, std::fs::metadata(&wal).unwrap().len());

    // And the diff between two samples is the real byte delta.
    let diff = first_cell(
        &mut s,
        &format!("SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0')"),
    );
    assert_eq!(diff.parse::<u64>().unwrap(), second);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn round476_no_wal_still_answers_zero() {
    // Without a WAL nothing is being written, and 0/0 is the truth rather
    // than a stub — the point of the change is not to invent a number.
    let dir = std::env::temp_dir().join(format!(
        "spg-e2e-wal-lsn-none-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(addrs.pgwire.as_ref().unwrap());
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "bench");
    assert_eq!(first_cell(&mut s, "SELECT pg_current_wal_lsn()"), "0/0");
    let _ = std::fs::remove_dir_all(&dir);
}
