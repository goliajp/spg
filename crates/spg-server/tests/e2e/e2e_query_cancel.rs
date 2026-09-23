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
    let p = crate::common::tmp_base().join(format!("spg-e2e-{tag}-{nanos}"));
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
                assert!(text.contains("user request"), "PG cancel text in {text:?}");
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

/// 9.0.4 — a statement asleep in `pg_sleep` is served outside the
/// engine lock, by the server's own slicing loop, and that loop used to
/// raise the cancellation as a spelled-out MESSAGE rather than the
/// `Cancelled` variant. A message goes through the catch-all, so the
/// client was told `42000` — the class drivers read as a programming
/// error and never retry — where PostgreSQL says `57014`.
///
/// Both reasons the token trips are pinned, because the variant is also
/// what lets them be told apart: a timeout keeps PostgreSQL's timeout
/// sentence, a cancel from elsewhere gets its "user request" one.
#[test]
fn statement_timeout_on_a_sleeping_statement_is_57014() {
    let dir = unique_tmpdir("qcancel3");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send_startup(&mut s, "anyone");
    let _ = handshake(&mut s);

    send_query(&mut s, "SET statement_timeout = '300ms'");
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }

    send_query(&mut s, "SELECT pg_sleep(5)");
    let mut said = None;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'E' => said = Some(String::from_utf8_lossy(&m.body).to_string()),
            b'Z' => break,
            _ => {}
        }
    }
    let said = said.expect("the sleep was never cut short");
    assert!(said.contains("57014"), "PG's query_canceled, got {said:?}");
    assert!(
        said.contains("statement timeout"),
        "PG's timeout sentence, got {said:?}"
    );

    // And the session is still usable, as it is after any ERROR.
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
    assert!(saw_row, "session unusable after the timeout");
}

/// 9.0.4 — the same loop, tripped by another connection's
/// CancelRequest instead of by the clock. PostgreSQL names the reason:
/// "canceling statement due to user request".
#[test]
fn cancel_request_on_a_sleeping_statement_names_the_user() {
    let dir = unique_tmpdir("qcancel4");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send_startup(&mut s, "anyone");
    let (pid, secret) = handshake(&mut s);

    let addr2 = addr.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send_cancel(&addr2, pid, secret);
    });
    send_query(&mut s, "SELECT pg_sleep(5)");
    let mut said = None;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'E' => said = Some(String::from_utf8_lossy(&m.body).to_string()),
            b'Z' => break,
            _ => {}
        }
    }
    canceller.join().unwrap();
    let said = said.expect("the sleep was never cancelled");
    assert!(said.contains("57014"), "PG's query_canceled, got {said:?}");
    assert!(
        said.contains("user request"),
        "PG names the sender, got {said:?}"
    );
}

/// 9.0.4 — `pg_terminate_backend` answers the victim's statement with
/// the FATAL, the way PostgreSQL does.
///
/// Terminate trips the cancel flag as well, so the victim used to be
/// told `57014 canceling statement due to statement timeout` — an
/// ordinary error. A client running one statement acts on it and
/// leaves, and the 57P01 the session loop sends at the next message
/// boundary arrives at a socket nobody is reading. Measured on
/// PostgreSQL 18.6, psql shows exactly one line:
/// `FATAL: 57P01: terminating connection due to administrator command`.
#[test]
fn terminate_answers_the_statement_with_the_fatal() {
    let dir = unique_tmpdir("qterm");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = ChildGuard(raw);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    let mut victim = TcpStream::connect(&addr).unwrap();
    victim
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    send_startup(&mut victim, "anyone");
    let (pid, _secret) = handshake(&mut victim);

    let addr2 = addr.clone();
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let mut k = TcpStream::connect(&addr2).unwrap();
        k.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        send_startup(&mut k, "anyone");
        let _ = handshake(&mut k);
        send_query(&mut k, &format!("SELECT pg_terminate_backend({pid})"));
        loop {
            if read_message(&mut k).ty == b'Z' {
                return;
            }
        }
    });

    send_query(&mut victim, "SELECT pg_sleep(5)");
    let m = loop {
        let m = read_message(&mut victim);
        if m.ty == b'E' {
            break m;
        }
        assert_ne!(m.ty, b'Z', "the sleep finished instead of being terminated");
    };
    killer.join().unwrap();
    let text = String::from_utf8_lossy(&m.body).to_string();
    assert!(
        text.contains("57P01"),
        "PostgreSQL's admin_shutdown, got {text:?}"
    );
    assert!(
        text.contains("FATAL"),
        "an ERROR tells a pool the session is still good; got {text:?}"
    );
    assert!(
        text.contains("terminating connection due to administrator command"),
        "PostgreSQL's sentence, got {text:?}"
    );
}
