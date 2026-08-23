//! v6.10.4 — `--replay-only` boot mode.
//!
//! Smoke test that the boot path can replay a WAL into an
//! engine and exit 0 without opening any listener. Two scopes:
//!
//!   1. Empty inputs: no db, no WAL → boots, replays nothing,
//!      exits 0.
//!   2. Populated inputs: a previous session wrote a snapshot +
//!      WAL; `--replay-only` restores both and exits 0.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_error_response};

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-replay-only-{label}-{nanos}"));
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

fn run_replay_only(db: &std::path::Path, wal: &std::path::Path) -> i32 {
    let status = Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg("127.0.0.1:0")
        .arg(db)
        .arg("-")
        .arg(wal)
        .arg("--replay-only")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn spg-server");
    status.code().unwrap_or(-1)
}

#[test]
fn replay_only_empty_inputs_exits_0() {
    let dir = unique_tmpdir("empty");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");
    let code = run_replay_only(&db, &wal);
    assert_eq!(code, 0, "empty inputs → clean exit 0");
}

#[test]
fn replay_only_populated_inputs_exits_0() {
    let dir = unique_tmpdir("populated");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");
    // Phase 1: write some state via a real server.
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .spawn();
        {
            let mut s = common::connect_to(&addrs.native);
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL)");
            for i in 0..5 {
                exec_native(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            }
            exec_native(&mut s, "CHECKPOINT");
            // Post-CHECKPOINT inserts land in the fresh WAL.
            for i in 5..10 {
                exec_native(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            }
        }
        graceful_stop(&mut raw);
    }
    // Phase 2: --replay-only should restore snapshot + WAL and
    // exit cleanly.
    let start = Instant::now();
    let code = run_replay_only(&db, &wal);
    let elapsed = start.elapsed();
    assert_eq!(code, 0, "populated inputs → clean exit 0");
    assert!(
        elapsed < Duration::from_secs(10),
        "--replay-only took {elapsed:?}; should be near-instant"
    );
}
