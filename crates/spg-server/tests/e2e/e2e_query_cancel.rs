//! v7.39 (query cancel) — the PG CancelRequest protocol: a second
//! connection echoing this session's BackendKeyData (pid, secret)
//! trips the in-flight statement, which fails with 57014 and PG's
//! "user request" text. A wrong secret is a no-op.

use crate::common;
use common::ChildGuard;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
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

/// Handshake until ReadyForQuery, capturing BackendKeyData.
fn handshake(s: &mut TcpStream) -> (u32, u32) {
    let auth = read_message(s);
    assert_eq!(auth.ty, b'R');
    let mut key = None;
    loop {
        let m = read_message(s);
        match m.ty {
            b'K' => {
                let pid = u32::from_be_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]);
                let secret = u32::from_be_bytes([m.body[4], m.body[5], m.body[6], m.body[7]]);
                key = Some((pid, secret));
            }
            b'Z' => return key.expect("BackendKeyData before ReadyForQuery"),
            _ => {}
        }
    }
}

fn send_cancel(addr: &str, pid: u32, secret: u32) {
    let mut c = TcpStream::connect(addr).unwrap();
    let mut pkt = Vec::with_capacity(16);
    pkt.extend_from_slice(&16u32.to_be_bytes());
    pkt.extend_from_slice(&80877102u32.to_be_bytes());
    pkt.extend_from_slice(&pid.to_be_bytes());
    pkt.extend_from_slice(&secret.to_be_bytes());
    c.write_all(&pkt).unwrap();
    // PG sends no response on the cancel connection.
}

#[test]
fn cancel_request_interrupts_running_statement() {
    let dir = unique_tmpdir("qcancel");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send_startup(&mut s, "anyone");
    let (pid, secret) = handshake(&mut s);

    // Fire the cancel shortly after the long scan starts.
    let addr2 = addr.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send_cancel(&addr2, pid, secret);
    });
    // Workload choice: each side stays under generate_series's 10M
    // materialisation cap while the nested-loop product (4e12) runs
    // effectively forever — the cancel must win, deterministically,
    // via the join loop's cancel checkpoints. (A single 200M series
    // used to work here, but parallel aggregation now reaches the 10M
    // cap error before a 300 ms cancel lands.)
    send_query(
        &mut s,
        "SELECT count(*) FROM generate_series(1, 2000000) a, generate_series(1, 2000000) b",
    );
    // Expect ErrorResponse with 57014 / "user request".
    let mut got_error = false;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'E' => {
                let text = String::from_utf8_lossy(&m.body).to_string();
                assert!(text.contains("57014"), "sqlstate in {text:?}");
                assert!(
                    text.contains("user request"),
                    "PG cancel text in {text:?}"
                );
                got_error = true;
            }
            b'Z' => break,
            _ => {}
        }
    }
    canceller.join().unwrap();
    assert!(got_error, "statement was not cancelled");

    // The session survives and runs the next statement normally.
    send_query(&mut s, "SELECT 1");
    let mut saw_row = false;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => saw_row = true,
            b'Z' => break,
            _ => {}
        }
    }
    assert!(saw_row, "session unusable after cancel");
}

#[test]
fn cancel_with_wrong_secret_is_a_noop() {
    let dir = unique_tmpdir("qcancel2");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send_startup(&mut s, "anyone");
    let (pid, secret) = handshake(&mut s);

    send_cancel(&addr, pid, secret.wrapping_add(1));
    // The statement AFTER a bad cancel runs to completion.
    send_query(&mut s, "SELECT 42");
    let mut saw_row = false;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => saw_row = true,
            b'E' => panic!("wrong-secret cancel must not affect the session"),
            b'Z' => break,
            _ => {}
        }
    }
    assert!(saw_row);
}
