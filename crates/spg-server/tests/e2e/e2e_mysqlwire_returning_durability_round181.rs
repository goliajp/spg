//! read01 round 181 — mysql-wire RETURNING durability (MariaDB
//! dialect `INSERT … RETURNING`) across kill + restart.
//!
//! Closes the last unpinned wire of the r178/r180 durability family:
//! both mysql-wire persist call sites route through the shared
//! `persist_wire_write`, whose pre-r178 CommandOk-only gate dropped
//! RETURNING (Rows) writes from the WAL. This pin proves the fix on
//! the mysql wire itself: COM_QUERY RETURNING writes ack, survive a
//! kill -9 and replay; writable-CTE DML via COM_QUERY too.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-mysql-ret-dur-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).expect("read header");
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let seqno = hdr[3];
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("read payload");
    (seqno, payload)
}

fn write_packet(stream: &mut TcpStream, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.push(seqno);
    out.extend_from_slice(payload);
    stream.write_all(&out).unwrap();
}

fn build_handshake_response(username: &str) -> Vec<u8> {
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

fn auth_open_mode(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _greeting) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_handshake_response("anyone"));
    let (_seqno, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "expected OK after auth, got {:#x}", ok[0]);
    s
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &payload);
}

/// Run a COM_QUERY; drain the full response. Returns the data-row
/// count for result sets (0 for OK packets); panics on ERR.
fn exec(s: &mut TcpStream, sql: &str, ctx: &str) -> usize {
    send_query(s, sql);
    let (_seq, first) = read_packet(s);
    match first[0] {
        0x00 => 0, // OK packet — no result set
        0xff => {
            let msg = String::from_utf8_lossy(&first[3..]).into_owned();
            panic!("[{ctx}] mysql exec failed for {sql:?}: {msg}");
        }
        _ => {
            // Result set: the first packet is the column count; then
            // exactly N column defs; then the marker closing them; then
            // row packets until the trailing marker.
            let col_count = u64::from(first[0]);
            for _ in 0..col_count {
                let _ = read_packet(s);
            }
            // v7.39 (round 504) — this client takes no CLIENT_DEPRECATE_EOF,
            // so an EOF closes the column definitions and another closes the
            // rows. SPG used to send neither, having framed against the
            // capabilities it advertised rather than the ones taken.
            let (_s, cols_eof) = read_packet(s);
            assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
            let mut rows = 0;
            loop {
                let (_s, p) = read_packet(s);
                if p[0] == 0xfe && p.len() <= 9 {
                    return rows;
                }
                if p[0] == 0xff {
                    let msg = String::from_utf8_lossy(&p[3..]).into_owned();
                    panic!("[{ctx}] mid-resultset error for {sql:?}: {msg}");
                }
                rows += 1;
            }
        }
    }
}

fn spawn_server(dir: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .with_mysqlwire()
        .spawn()
}

#[test]
fn mysqlwire_returning_and_cte_survive_kill_restart() {
    let dir = unique_tmpdir();
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let addr = addrs.mysqlwire.clone().expect("mysql-wire addr");
        let mut s = auth_open_mode(&addr);
        exec(&mut s, "CREATE TABLE t (id BIGINT, v BIGINT)", "ddl");
        // MariaDB-dialect RETURNING over COM_QUERY.
        let n = exec(
            &mut s,
            "INSERT INTO t VALUES (1, 0) RETURNING id",
            "ins-ret",
        );
        assert_eq!(n, 1, "RETURNING must yield the row over mysql-wire");
        exec(
            &mut s,
            "UPDATE t SET v = 9 WHERE id = 1 RETURNING id",
            "upd-ret",
        );
        // Writable CTE via COM_QUERY.
        exec(
            &mut s,
            "WITH src AS (SELECT 2 AS x) INSERT INTO t SELECT x, 0 FROM src",
            "cte",
        );
        let n = exec(&mut s, "SELECT id FROM t", "verify");
        assert_eq!(n, 2, "pre-kill row count");
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    let mut s = auth_open_mode(&addr);
    assert_eq!(
        exec(&mut s, "SELECT id FROM t", "post-restart"),
        2,
        "mysql-wire RETURNING + CTE writes must survive kill + replay"
    );
    assert_eq!(
        exec(&mut s, "SELECT id FROM t WHERE v = 9", "post-restart-upd"),
        1,
        "RETURNING UPDATE must replay"
    );
}
