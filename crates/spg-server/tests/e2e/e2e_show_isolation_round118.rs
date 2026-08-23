//! v7.39 (read01 round 118, B3) — `SHOW transaction_isolation` reports the
//! live level over the wire. It used to be canned to "read committed" in the
//! pgwire layer, so `BEGIN ISOLATION LEVEL REPEATABLE READ; SHOW
//! transaction_isolation` wrongly reported "read committed". The canned
//! response is gone; the query now reaches the engine, which reads the live
//! `current_isolation_level` set by `BEGIN ISOLATION LEVEL …` and reverts to
//! the default at COMMIT / ROLLBACK. Verified over the real pgwire protocol.
//!
//! (Concurrent per-connection isolation is still gated on per-connection TxId —
//! a separate RFC. This test uses one connection.)

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-showiso-{label}-{nanos}"));
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
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

/// First text cell of the first DataRow (`D`) of `sql`'s reply.
fn first_cell(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let d = msgs
        .iter()
        .find(|m| m.ty == b'D')
        .unwrap_or_else(|| panic!("no DataRow for {sql}"));
    // DataRow: int16 field count, then per field int32 len + bytes.
    let body = &d.body;
    let n = u16::from_be_bytes([body[0], body[1]]);
    assert!(n >= 1, "empty DataRow for {sql}");
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    assert!(len >= 0, "NULL cell for {sql}");
    let start = 6;
    let end = start + len as usize;
    String::from_utf8_lossy(&body[start..end]).into_owned()
}

fn run_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "unexpected error for {sql}"
    );
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn show_transaction_isolation_reports_live_level() {
    let dir = unique_tmpdir("live");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );

    run_ok(&mut s, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "repeatable read"
    );
    run_ok(&mut s, "COMMIT");
    // Reverts to the default at transaction end.
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );

    run_ok(&mut s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "serializable"
    );
    // PG's multi-word spelling reports the live level too.
    assert_eq!(
        first_cell(&mut s, "SHOW TRANSACTION ISOLATION LEVEL"),
        "serializable"
    );
    run_ok(&mut s, "ROLLBACK");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );
    assert_eq!(
        first_cell(&mut s, "SHOW TRANSACTION ISOLATION LEVEL"),
        "read committed"
    );
}
