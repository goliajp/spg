//! v6.5.5 — Wait events lite. Only `write_lock` is wired in
//! v6.5.5; fsync + group_commit instrumentation lives across
//! thread boundaries and is carved out per STABILITY.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-wait-{label}-{nanos}"));
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

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn exec_simple(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let _ = read_until_ready(s);
}

#[test]
fn wait_event_is_idle_between_queries() {
    let dir = unique_tmpdir("idle");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE t (id INT)");
    // Pause briefly to ensure the prior query has fully completed
    // and wait_event has been cleared.
    std::thread::sleep(Duration::from_millis(10));

    // Query spg_stat_activity from another connection so we see
    // OUR wait_event from a clean read.
    let mut probe = open(addrs.pgwire.as_ref().unwrap());
    send_query(&mut probe, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut probe);
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert!(
        !data_rows.is_empty(),
        "expected ≥1 row, got {}",
        data_rows.len()
    );
    // v7.37.14 (B6.3) — wait_event column is now index 5 (was 4
    // pre-v7.37.14; the new `wait_event_type` lives at index 4 so
    // we skip ONE more cell to reach wait_event). Every row's
    // wait_event should be empty (idle) since no one is mid-query
    // (the probe is mid-query but that's the SELECT path which is
    // also idle post-engine.read).
    for dr in &data_rows {
        // Skip cells 0..5 to land at cell 5 = wait_event.
        let mut off = 2;
        for _ in 0..5 {
            let len = i32::from_be_bytes([
                dr.body[off],
                dr.body[off + 1],
                dr.body[off + 2],
                dr.body[off + 3],
            ]) as usize;
            off += 4 + len;
        }
        let we_len = i32::from_be_bytes([
            dr.body[off],
            dr.body[off + 1],
            dr.body[off + 2],
            dr.body[off + 3],
        ]) as usize;
        let we = std::str::from_utf8(&dr.body[off + 4..off + 4 + we_len]).unwrap();
        // Either "" (idle) or "write_lock" if caught mid-execute.
        // The point: it's not garbage.
        assert!(
            we.is_empty() || we == "write_lock",
            "unexpected wait_event {we:?}"
        );
    }
}
