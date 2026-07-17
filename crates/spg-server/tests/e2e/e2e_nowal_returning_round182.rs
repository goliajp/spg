//! read01 round 182 — RETURNING DML persists in no-WAL snapshot mode.
//!
//! Last member of the r178/r180 CommandOk-only gate family: a server
//! running with a db path but NO WAL persists by snapshotting after
//! each mutating statement — and that gate also matched CommandOk
//! only, so an acked `INSERT … RETURNING` (Rows) vanished on restart.
//! Pins both wires (native + pgwire) against a kill + reboot.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, build_query, encode, parse_data_row, parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-nowal-ret-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// db path only — snapshot persistence, no WAL.
fn spawn_server(dir: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn()
}

fn nat_send(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn nat_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn nat_exec(stream: &mut TcpStream, sql: &str) {
    nat_send(stream, sql);
    loop {
        let f = nat_frame(stream);
        match f.op {
            Op::CommandComplete => return,
            Op::RowDescription | Op::DataRow | Op::DataRowBatch => {}
            other => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("native exec failed for {sql:?}: {other:?} {msg}");
            }
        }
    }
}

fn nat_count(stream: &mut TcpStream, sql: &str) -> usize {
    nat_send(stream, sql);
    assert_eq!(nat_frame(stream).op, Op::RowDescription);
    let mut n = 0;
    loop {
        let f = nat_frame(stream);
        match f.op {
            Op::DataRow => n += parse_data_row(&f).map(|_| 1).unwrap_or(1),
            Op::DataRowBatch => n += parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => return n,
            other => panic!("unexpected {other:?}"),
        }
    }
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

fn pg_exec(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut err: Option<String> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'E' => err = Some(String::from_utf8_lossy(&body).into_owned()),
            b'Z' => {
                if let Some(e) = err {
                    panic!("pgwire exec failed for {sql:?}: {e}");
                }
                return;
            }
            _ => {}
        }
    }
}

#[test]
fn nowal_returning_survives_restart_both_wires() {
    let dir = unique_tmpdir("both");
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut nat = common::connect_to(&addrs.native);
        nat.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        nat_exec(&mut nat, "CREATE TABLE t (id BIGINT, v BIGINT)");
        // pgwire RETURNING write first — its own snapshot coverage
        // (r178's persist_wire_write fix) must not be what saves the
        // later native rows.
        let mut pg = pg_connect(addrs.pgwire.as_ref().unwrap());
        pg_exec(&mut pg, "INSERT INTO t VALUES (2, 0) RETURNING id");
        // Native wire RETURNING writes LAST before the kill: nothing
        // after them may trigger a snapshot, or the audit can't tell
        // whether THIS statement persisted itself.
        nat_exec(&mut nat, "INSERT INTO t VALUES (1, 0) RETURNING id");
        nat_exec(&mut nat, "UPDATE t SET v = 9 WHERE id = 1 RETURNING id");
        assert_eq!(nat_count(&mut nat, "SELECT id FROM t"), 2);
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut nat = common::connect_to(&addrs.native);
    nat.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        nat_count(&mut nat, "SELECT id FROM t"),
        2,
        "no-WAL snapshot mode must persist RETURNING writes from both wires"
    );
    assert_eq!(
        nat_count(&mut nat, "SELECT id FROM t WHERE v = 9"),
        1,
        "RETURNING UPDATE must persist"
    );
}
