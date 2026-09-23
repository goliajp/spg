//! 9.0.4 — a graceful stop names itself to the sessions it ends.
//!
//! The drain used to be silent: it stopped accepting, waited for the
//! sessions to finish, and exited. A client that was writing saw only
//! its socket disappear, which a pool cannot tell apart from the
//! network failing mid-COMMIT. PostgreSQL's fast shutdown sends
//! `FATAL 57P01 terminating connection due to administrator command`,
//! and that is what an idle-in-pool connection and a working one both
//! have to receive here.

use crate::common;
use common::ChildGuard;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

/// Read until ReadyForQuery, discarding what comes before it.
fn drain_to_ready(s: &mut TcpStream) {
    loop {
        let mut header = [0u8; 5];
        s.read_exact(&mut header).expect("pg header");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        if !body.is_empty() {
            s.read_exact(&mut body).expect("pg body");
        }
        if header[0] == b'Z' {
            return;
        }
    }
}

/// Everything the server says from now until it closes the socket.
fn read_until_eof(s: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[allow(unsafe_code)]
fn send_sigterm(child: &Child) {
    let pid = i32::try_from(child.id()).expect("pid fits in i32");
    // SAFETY: `kill(2)` with a live pid and a valid signal number. The
    // server installs a SIGTERM handler that flips the drain flag.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill: {}", std::io::Error::last_os_error());
}

#[test]
fn a_graceful_stop_tells_the_sessions_it_ends() {
    let dir = unique_tmpdir("shutdown-tells");
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    // Two sessions, the two a pool holds: one that has just finished a
    // statement, and one that has been sitting idle since it opened.
    let mut working = TcpStream::connect(&addr).unwrap();
    working
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    send_startup(&mut working, "anyone");
    drain_to_ready(&mut working);
    send_query(&mut working, "SELECT 1");
    drain_to_ready(&mut working);

    let mut idle = TcpStream::connect(&addr).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    send_startup(&mut idle, "anyone");
    drain_to_ready(&mut idle);

    send_sigterm(&child.0);

    for (what, s) in [
        ("the working session", &mut working),
        ("the idle one", &mut idle),
    ] {
        let said = read_until_eof(s);
        assert!(
            said.contains("57P01"),
            "{what} was dropped without PostgreSQL's shutdown SQLSTATE; it got {said:?}"
        );
        assert!(
            said.contains("terminating connection due to administrator command"),
            "{what} got no sentence naming the stop; it got {said:?}"
        );
    }
}
