//! read01 round 178 — pgwire plain autocommit DML rides the commit
//! barrier (group fsync), sharing the native wrap path's leader.
//!
//! Pins:
//!   1. Queue-routed DML is durable: INSERTs over pgwire survive
//!      kill -9 + WAL replay (the leader's v3 group framing).
//!   2. Exclusions keep working inline: RETURNING gives rows back,
//!      BEGIN…COMMIT transactions commit, and both survive restart.
//!   3. Concurrent pgwire writers all land (group fan-out
//!      correctness) and survive restart.

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
    let p = crate::common::tmp_base().join(format!("spg-pgwire-gc-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(dir: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .with_pgwire()
        .spawn()
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) {
    loop {
        if read_message(s).ty == b'Z' {
            return;
        }
    }
}

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    read_until_ready(&mut s);
    s
}

/// Run one simple query; return (data_row_count, saw_error).
fn run_query(s: &mut TcpStream, sql: &str) -> (usize, bool) {
    send_query(s, sql);
    let mut rows = 0;
    let mut err = false;
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => rows += 1,
            b'E' => err = true,
            b'Z' => return (rows, err),
            _ => {}
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    let (_, err) = run_query(s, sql);
    assert!(!err, "query failed: {sql}");
}

#[test]
fn queued_dml_survives_kill_and_replay() {
    let dir = unique_tmpdir("durable");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        exec_ok(&mut s, "CREATE TABLE t (id BIGINT)");
        for i in 0..30_i64 {
            exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
        let (n, _) = run_query(&mut s, "SELECT id FROM t");
        assert_eq!(n, 30, "queued inserts visible");
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    let (n, _) = run_query(&mut s, "SELECT id FROM t");
    assert_eq!(n, 30, "acked queued DML must survive kill + replay");
}

#[test]
fn returning_and_tx_exclusions_stay_correct() {
    let dir = unique_tmpdir("excl");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        exec_ok(&mut s, "CREATE TABLE t (id BIGINT)");
        let (rows, err) = run_query(&mut s, "INSERT INTO t VALUES (1) RETURNING id");
        assert!(!err, "RETURNING insert must succeed");
        assert_eq!(rows, 1, "RETURNING must yield the row");
        exec_ok(&mut s, "BEGIN");
        exec_ok(&mut s, "INSERT INTO t VALUES (2)");
        exec_ok(&mut s, "INSERT INTO t VALUES (3)");
        exec_ok(&mut s, "COMMIT");
        let (n, _) = run_query(&mut s, "SELECT id FROM t");
        assert_eq!(n, 3);
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    let (n, _) = run_query(&mut s, "SELECT id FROM t");
    assert_eq!(n, 3, "RETURNING + tx rows must survive restart");
}

#[test]
fn concurrent_pgwire_writers_all_land() {
    let dir = unique_tmpdir("conc");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let pg_addr = addrs.pgwire.clone().unwrap();
        {
            let mut s = pg_connect(&pg_addr);
            exec_ok(&mut s, "CREATE TABLE t (id BIGINT)");
        }
        let handles: Vec<_> = (0..4)
            .map(|w| {
                let addr = pg_addr.clone();
                std::thread::spawn(move || {
                    let mut s = pg_connect(&addr);
                    for i in 0..25_i64 {
                        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({})", w * 100 + i));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut s = pg_connect(&pg_addr);
        let (n, _) = run_query(&mut s, "SELECT id FROM t");
        assert_eq!(n, 100, "all concurrent writers' rows visible");
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    let (n, _) = run_query(&mut s, "SELECT id FROM t");
    assert_eq!(n, 100, "all concurrent writers' rows survive restart");
}
