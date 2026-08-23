//! read01 round 191 (M24) — file-access failures carry PG's
//! errcode_for_file_access() SQLSTATEs on the wire, verified with
//! the r190 chaos knobs:
//!   * injected fsync EIO      → 58030 io_error
//!   * WAL quota (disk full)   → 53100 disk_full
//! The mapper arms existed; this pins them end-to-end over pgwire
//! (ErrorResponse 'C' field), where ORMs and retry logic branch.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-file-sqlstate-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(
    dir: &std::path::Path,
    env: &[(&str, &str)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .with_pgwire();
    for (k, v) in env {
        b = b.env(*k, (*v).to_string());
    }
    b.spawn()
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
    body.extend_from_slice(&196608u32.to_be_bytes());
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

/// Run one simple query; return Some(SQLSTATE) from the first
/// ErrorResponse (its 'C' field), None on success.
fn exec_sqlstate(s: &mut TcpStream, sql: &str) -> Option<String> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut code = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'E' => {
                // ErrorResponse: fields are (tag byte, cstring)*, 0.
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
                    if tag == b'C' {
                        code = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => return code,
            _ => {}
        }
    }
}

#[test]
fn injected_fsync_failure_is_58030() {
    let dir = unique_tmpdir("eio");
    // fsync #1 = CREATE TABLE, #2 = INSERT (injected).
    let (raw, addrs) = spawn_server(&dir, &[("SPG_FAIL_FSYNC_AT", "2")]);
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec_sqlstate(&mut s, "CREATE TABLE t (id INT)"), None);
    assert_eq!(
        exec_sqlstate(&mut s, "INSERT INTO t VALUES (1)").as_deref(),
        Some("58030"),
        "injected fsync EIO must map to io_error"
    );
}

#[test]
fn wal_quota_exhaustion_is_53100() {
    let dir = unique_tmpdir("quota");
    let (raw, addrs) = spawn_server(
        &dir,
        &[
            ("SPG_FAIL_WAL_QUOTA_BYTES", "200"),
            ("SPG_DISABLE_WAL_PREFLIGHT", "1"),
        ],
    );
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec_sqlstate(&mut s, "CREATE TABLE t (id INT)"), None);
    // Append until the 200-byte quota rejects one.
    let mut saw = None;
    for i in 0..10 {
        if let Some(code) = exec_sqlstate(&mut s, &format!("INSERT INTO t VALUES ({i})")) {
            saw = Some(code);
            break;
        }
    }
    assert_eq!(
        saw.as_deref(),
        Some("53100"),
        "WAL quota exhaustion must map to disk_full"
    );
}
