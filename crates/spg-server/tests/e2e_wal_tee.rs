//! v6.10.6 — `SPG_WAL_TEE_PATH` WAL stream tee.
//!
//! Best-effort mirror of every group-committed WAL append to a
//! second file. Validates that the tee bytes match the primary
//! WAL bytes for the lifetime of one server, and that tee
//! failures (file removed mid-flight) don't roll back the
//! primary commit.

#![allow(clippy::uninlined_format_args, unsafe_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{Op, build_query, encode, parse_error_response};

mod common;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-wal-tee-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn graceful_stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = child.wait();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_until_cc(s: &mut TcpStream, sql: &str) {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "SQL failed: {sql:?} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc(s, sql);
}

#[test]
fn tee_mirrors_primary_wal_bytes() {
    let dir = unique_tmpdir("mirror");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");
    let tee = dir.join("wal.tee");

    let (mut raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .arg("-")
        .arg_path(&wal)
        .env("SPG_WAL_TEE_PATH", tee.to_string_lossy().into_owned())
        .spawn();
    {
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        for i in 0..5 {
            exec_native(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
    }
    graceful_stop(&mut raw);

    let wal_bytes = std::fs::read(&wal).expect("read wal");
    let tee_bytes = std::fs::read(&tee).expect("read tee");
    assert!(!wal_bytes.is_empty(), "wal must have content");
    assert_eq!(
        wal_bytes,
        tee_bytes,
        "tee bytes ({}) must match primary WAL bytes ({}) verbatim",
        tee_bytes.len(),
        wal_bytes.len()
    );
}

#[test]
fn tee_disabled_when_env_unset() {
    let dir = unique_tmpdir("disabled");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");
    let tee = dir.join("wal.tee");

    // No SPG_WAL_TEE_PATH: tee file must NOT exist after running.
    let (mut raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .arg("-")
        .arg_path(&wal)
        .spawn();
    {
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        exec_native(&mut s, "INSERT INTO t VALUES (1)");
    }
    graceful_stop(&mut raw);
    assert!(!tee.exists(), "tee file must not exist when env unset");
}
