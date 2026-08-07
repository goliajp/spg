//! read01 round 172 — SQL `SET synchronous_commit` is a real session
//! control on the server write paths (PG semantics).
//!
//! Before r172 the server stored + SHOWed the GUC but the WAL write
//! paths only consulted the `SPG_SYNCHRONOUS_COMMIT` env var — the
//! standard PG latency lever (`SET synchronous_commit = off`) was
//! silently inert over the wire. r172 wires the session value into
//! both the auto-commit group-commit path (leader fsync) and the
//! per-statement append path, with the (now always-spawned, dormant
//! until needed) flusher bounding the async loss window.
//!
//! Pins:
//!   1. `SET synchronous_commit = off` is accepted, writes still
//!      apply + are visible, and toggling back to `on` keeps working.
//!   2. Async-committed rows survive a kill + restart when the
//!      appends completed before the kill: `write_all` puts the WAL
//!      bytes in the kernel before the client ack, so a process kill
//!      (not a power loss) loses nothing already acked.
//!   3. Invalid values are rejected with an error (PG value domain,
//!      validated engine-side since r171).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use std::process::Child;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_command_complete,
    parse_data_row, parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-session-sync-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(db: &std::path::Path, wal: &std::path::Path) -> (Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .spawn()
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
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

fn expect_cc(stream: &mut TcpStream) {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn run_select(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => rows.extend(parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return rows,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[test]
fn session_set_sync_commit_off_writes_apply_and_toggle_back() {
    let dir = unique_tmpdir("toggle");
    let db = dir.join("data.spgdb");
    let wal = dir.join("data.wal");
    let (raw, addrs) = spawn_server(&db, &wal);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "CREATE TABLE rows (id BIGINT, name TEXT)");
    expect_cc(&mut s);
    send_query(&mut s, "SET synchronous_commit = off");
    expect_cc(&mut s);
    for i in 0..5_i64 {
        send_query(
            &mut s,
            &format!("INSERT INTO rows VALUES ({i}, 'async-{i}')"),
        );
        expect_cc(&mut s);
    }
    assert_eq!(
        run_select(&mut s, "SELECT id FROM rows").len(),
        5,
        "async-commit inserts must be visible immediately"
    );

    // Toggle back to sync; both modes keep committing on one
    // connection within one server lifetime.
    send_query(&mut s, "SET synchronous_commit = on");
    expect_cc(&mut s);
    send_query(&mut s, "INSERT INTO rows VALUES (5, 'sync-again')");
    expect_cc(&mut s);
    assert_eq!(
        run_select(&mut s, "SELECT id FROM rows").len(),
        6,
        "post-toggle sync insert must land on top of the async ones"
    );
}

#[test]
fn session_async_commits_survive_restart() {
    // All appends complete (write_all into the kernel) before the
    // client ack, so a process kill after the last ack loses
    // nothing — only a power/OS loss could. Restart must replay
    // every acked row.
    let dir = unique_tmpdir("restart");
    let db = dir.join("data.spgdb");
    let wal = dir.join("data.wal");
    {
        let (raw, addrs) = spawn_server(&db, &wal);
        let mut guard = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (id BIGINT)");
        expect_cc(&mut s);
        send_query(&mut s, "SET synchronous_commit = off");
        expect_cc(&mut s);
        for i in 0..7_i64 {
            send_query(&mut s, &format!("INSERT INTO t VALUES ({i})"));
            expect_cc(&mut s);
        }
        assert_eq!(run_select(&mut s, "SELECT id FROM t").len(), 7);
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    let (raw, addrs) = spawn_server(&db, &wal);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        run_select(&mut s, "SELECT id FROM t").len(),
        7,
        "acked async commits must survive process kill + WAL replay"
    );
}

#[test]
fn session_sync_commit_rejects_invalid_value() {
    let dir = unique_tmpdir("invalid");
    let db = dir.join("data.spgdb");
    let wal = dir.join("data.wal");
    let (raw, addrs) = spawn_server(&db, &wal);
    let _guard = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut s, "SET synchronous_commit = banana");
    let f = read_frame(&mut s);
    assert_ne!(
        f.op,
        Op::CommandComplete,
        "invalid synchronous_commit value must error, not succeed"
    );
    let msg = spg_wire::parse_error_response(&f).expect("error frame decodes");
    assert!(
        msg.contains("synchronous_commit"),
        "error should name the GUC, got: {msg}"
    );
    // Connection stays usable and the GUC still works afterwards.
    send_query(&mut s, "SET synchronous_commit = off");
    expect_cc(&mut s);
}
