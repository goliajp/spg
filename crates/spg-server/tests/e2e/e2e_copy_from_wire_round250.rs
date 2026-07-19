//! v7.39 (round 250) — `COPY … FROM STDIN` over the pgwire, probed
//! against live PG18.4 (2026-07-19). The wire COPY-in path lowered each
//! row to an INSERT via `engine.execute` directly, which left two
//! data-integrity holes the normal statement path doesn't have:
//!
//!   * NO WAL append — on a WAL-configured server every COPY'd row was
//!     acknowledged (`COPY n` + ReadyForQuery) and then lost on kill -9,
//!     because replay is the SQL-text log and COPY rows never entered
//!     it (the r178/r180 lesson, third spelling);
//!   * NO transaction wrap — a bad row mid-COPY left the earlier rows
//!     inserted, where PG's COPY is all-or-nothing.
//!
//! The fix drives the same BEGIN / per-row / COMMIT-or-ROLLBACK
//! sequence through both the engine and the WAL, so a crash mid-COPY
//! replays to the end-of-WAL auto-rollback (0 rows — exactly PG: the
//! client never saw success), and a bad row rolls the whole COPY back.
//! Inside a client's explicit transaction the COPY joins it unwrapped,
//! like any other statement.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-copywire-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

/// Run a non-COPY statement to ReadyForQuery; returns the first
/// ErrorResponse (code, message) if one arrived.
fn exec(s: &mut TcpStream, sql: &str) -> Option<(String, String)> {
    send_query(s, sql);
    read_to_ready(s)
}

fn read_to_ready(s: &mut TcpStream) -> Option<(String, String)> {
    let mut found = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'E' => found = Some(parse_error(&body)),
            b'Z' => return found,
            _ => {}
        }
    }
}

fn parse_error(body: &[u8]) -> (String, String) {
    let (mut code, mut msg) = (String::new(), String::new());
    let mut pos = 0;
    while pos < body.len() && body[pos] != 0 {
        let tag = body[pos];
        pos += 1;
        let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        let val = String::from_utf8_lossy(&body[pos..end]).into_owned();
        match tag {
            b'C' => code = val,
            b'M' => msg = val,
            _ => {}
        }
        pos = end + 1;
    }
    (code, msg)
}

fn first_cell(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let mut cell = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                // DataRow: u16 ncols, then per-col i32 len + bytes.
                let n = u16::from_be_bytes([body[0], body[1]]);
                assert!(n >= 1);
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                cell = Some(if len < 0 {
                    String::from("NULL")
                } else {
                    String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned()
                });
            }
            b'Z' => return cell.expect("no DataRow"),
            _ => {}
        }
    }
}

/// Drive one COPY … FROM STDIN: expects CopyInResponse, sends `data`
/// as one CopyData frame + CopyDone, reads to ReadyForQuery. Returns
/// the CommandComplete tag or the error (code, message).
fn copy_in(s: &mut TcpStream, sql: &str, data: &str) -> Result<String, (String, String)> {
    send_query(s, sql);
    // Wait for CopyInResponse (or an immediate error).
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'G' => break,
            b'E' => {
                let err = parse_error(&body);
                read_to_ready_after_err(s);
                return Err(err);
            }
            b'Z' => panic!("ReadyForQuery before CopyInResponse for {sql}"),
            _ => {}
        }
    }
    // CopyData + CopyDone.
    let mut out = Vec::new();
    out.push(b'd');
    out.extend_from_slice(&((data.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(data.as_bytes());
    out.push(b'c');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();
    // CommandComplete or ErrorResponse, then ReadyForQuery.
    let mut tag = None;
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'C' => {
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                tag = Some(String::from_utf8_lossy(&body[..end]).into_owned());
            }
            b'E' => err = Some(parse_error(&body)),
            b'Z' => {
                return match (tag, err) {
                    (_, Some(e)) => Err(e),
                    (Some(t), None) => Ok(t),
                    (None, None) => panic!("neither CommandComplete nor error for {sql}"),
                };
            }
            _ => {}
        }
    }
}

fn read_to_ready_after_err(s: &mut TcpStream) {
    loop {
        if pg_msg(s).0 == b'Z' {
            break;
        }
    }
}

#[test]
fn a_bad_row_rolls_the_whole_copy_back() {
    let dir = unique_dir("atomic");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec(&mut s, "CREATE TABLE ct (id int, name text, v int)"), None);
    // Row 1 is fine, row 2 is not: PG inserts NOTHING (COPY is one
    // command). The error is the bare cell error (22P02), not an
    // internal "COPY row INSERT failed:" wrapper.
    let err = copy_in(
        &mut s,
        "COPY ct FROM STDIN WITH (FORMAT csv)",
        "1,a,10\n2,b,notanint\n",
    )
    .expect_err("bad row must fail the COPY");
    assert_eq!(err.0, "22P02", "{}", err.1);
    assert!(err.1.contains("invalid input syntax for type integer: \"notanint\""), "{}", err.1);
    assert!(!err.1.contains("COPY row INSERT failed"), "{}", err.1);
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "0");
    // The connection is healthy afterwards and a clean COPY lands.
    let tag = copy_in(&mut s, "COPY ct FROM STDIN WITH (FORMAT csv)", "1,a,10\n2,b,\n").unwrap();
    assert_eq!(tag, "COPY 2");
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "2");
}

#[test]
fn copied_rows_survive_kill9_on_a_wal_server() {
    let dir = unique_dir("wal");
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    let count_after_restart;
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let mut guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        assert_eq!(exec(&mut s, "CREATE TABLE ct (id int, name text)"), None);
        let tag = copy_in(&mut s, "COPY ct FROM STDIN", "1\ta\n2\tb\n3\tc\n").unwrap();
        assert_eq!(tag, "COPY 3");
        // The server said COPY 3 — that is the durability promise.
        // kill -9, no shutdown path.
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let _guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        count_after_restart = first_cell(&mut s, "SELECT count(*) FROM ct");
    }
    assert_eq!(count_after_restart, "3", "COPY'd rows lost across kill -9");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_mid_copy_replays_to_zero_rows() {
    // The WAL carries BEGIN + the row INSERTs as the COPY streams; a
    // crash before COMMIT must replay to nothing (end-of-WAL
    // auto-rollback) — the client never saw success. Simulate by
    // killing the server right after a COPY that never sent CopyDone.
    let dir = unique_dir("midcopy");
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let mut guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        assert_eq!(exec(&mut s, "CREATE TABLE ct (id int)"), None);
        send_query(&mut s, "COPY ct FROM STDIN");
        loop {
            if pg_msg(&mut s).0 == b'G' {
                break;
            }
        }
        // Stream one row, never CopyDone.
        let data = b"1\n";
        let mut out = Vec::new();
        out.push(b'd');
        out.extend_from_slice(&((data.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(data);
        s.write_all(&out).unwrap();
        // Give the server a moment to process the frame, then kill.
        std::thread::sleep(Duration::from_millis(300));
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let _guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "0");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copy_inside_an_explicit_transaction_joins_it() {
    let dir = unique_dir("tx");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec(&mut s, "CREATE TABLE ct (id int)"), None);
    assert_eq!(exec(&mut s, "BEGIN"), None);
    let tag = copy_in(&mut s, "COPY ct FROM STDIN", "1\n2\n").unwrap();
    assert_eq!(tag, "COPY 2");
    // Client ROLLBACK discards the COPY — PG semantics.
    assert_eq!(exec(&mut s, "ROLLBACK"), None);
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "0");
    // And COMMIT keeps it.
    assert_eq!(exec(&mut s, "BEGIN"), None);
    copy_in(&mut s, "COPY ct FROM STDIN", "1\n2\n").unwrap();
    assert_eq!(exec(&mut s, "COMMIT"), None);
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "2");
}
