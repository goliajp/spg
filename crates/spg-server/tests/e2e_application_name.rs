//! v7.17 Phase 2.4 — `application_name` startup-param + `SET` wiring
//! surfacing through `spg_stat_activity.application_name` and `SHOW`.
//!
//! Three angles:
//!   1. Startup-param `application_name = 'foo'` → row.application_name == "foo".
//!   2. `SET application_name = 'bar'` → row.application_name flips to "bar".
//!   3. `SHOW application_name` returns the current value, both pre-SET
//!      (startup seed) and post-SET (updated GUC).

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-appname-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

/// Pack key/value pairs into a startup message body. Always writes
/// `user` first; appends every (k, v) in `extra` after it, then the
/// final empty key terminator.
fn send_startup_full(s: &mut TcpStream, user: &str, extra: &[(&str, &str)]) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    for (k, v) in extra {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0); // empty key terminator
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    let len = (body.len() + 4) as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    write_msg(&mut out, b'Q', &body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn open_with(addr: &str, user: &str, extra: &[(&str, &str)]) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup_full(&mut s, user, extra);
    let _ = read_until_ready(&mut s);
    s
}

/// Extract one column from a DataRow body at `col_idx`. PG wire DataRow:
///   u16 cell_count, then per cell: i32 length (-1 == NULL) + length bytes.
fn datarow_cell(body: &[u8], col_idx: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col_idx >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col_idx {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col_idx {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

/// Returns the cell value at column index `col` from the first DataRow
/// where column index `key_col` equals `key_val`. We match on `user`
/// (column 1) to find OUR connection row in `spg_stat_activity`.
fn find_row_cell(msgs: &[PgMessage], key_col: usize, key_val: &str, col: usize) -> Option<String> {
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        if let Some(k) = datarow_cell(&m.body, key_col) {
            if k == key_val {
                return datarow_cell(&m.body, col);
            }
        }
    }
    None
}

#[test]
fn startup_param_application_name_surfaces_in_activity() {
    let dir = unique_tmpdir("startup");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open_with(
        addrs.pgwire.as_ref().unwrap(),
        "alice",
        &[("application_name", "psql-alice")],
    );

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    // user column index = 1; application_name column index = 7 (last).
    let appname = find_row_cell(&msgs, 1, "alice", 7).expect("alice row + appname");
    assert_eq!(appname, "psql-alice");
}

#[test]
fn set_application_name_updates_activity_row() {
    let dir = unique_tmpdir("set");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open_with(addrs.pgwire.as_ref().unwrap(), "bob", &[]);

    // Pre-SET: empty.
    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs0 = read_until_ready(&mut s);
    let pre = find_row_cell(&msgs0, 1, "bob", 7).expect("bob row");
    assert_eq!(
        pre, "",
        "pre-SET application_name is empty when no startup param"
    );

    // SET it.
    send_query(&mut s, "SET application_name = 'pytest-runner'");
    let _ = read_until_ready(&mut s);

    // Post-SET: updated.
    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs1 = read_until_ready(&mut s);
    let post = find_row_cell(&msgs1, 1, "bob", 7).expect("bob row after SET");
    assert_eq!(post, "pytest-runner");
}

#[test]
fn show_application_name_reads_startup_then_set() {
    let dir = unique_tmpdir("show");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open_with(
        addrs.pgwire.as_ref().unwrap(),
        "carol",
        &[("application_name", "dbeaver")],
    );

    // SHOW after startup → seeded value.
    send_query(&mut s, "SHOW application_name");
    let msgs0 = read_until_ready(&mut s);
    let v0 = msgs0
        .iter()
        .filter(|m| m.ty == b'D')
        .find_map(|m| datarow_cell(&m.body, 0))
        .expect("SHOW DataRow");
    assert_eq!(v0, "dbeaver");

    // SHOW after SET → new value.
    send_query(&mut s, "SET application_name = 'reporter'");
    let _ = read_until_ready(&mut s);
    send_query(&mut s, "SHOW application_name");
    let msgs1 = read_until_ready(&mut s);
    let v1 = msgs1
        .iter()
        .filter(|m| m.ty == b'D')
        .find_map(|m| datarow_cell(&m.body, 0))
        .expect("SHOW DataRow");
    assert_eq!(v1, "reporter");
}
