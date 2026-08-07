//! v7.39 (round 548) — creating a group role locked the operator out.
//!
//! The wire decides between open mode and password auth once per
//! connection, and the rule was "does the engine hold ANY role". So a
//! `CREATE ROLE devs NOLOGIN` — a role that records no password and
//! cannot log in — flipped the whole server to password auth. The
//! bootstrap `postgres` identity has no password, so after that
//! nothing could connect, and there is no way back through SQL from a
//! database you cannot reach.
//!
//! The rule is now "does the engine hold a LOGIN role that a password
//! was actually DECLARED for". A bare `CREATE ROLE` still gets an
//! unguessable credential derived from its own salt — so no record
//! carries an empty password — which is exactly why "has a hash" could
//! not tell a real account from a group role, and why the declaration
//! is recorded (user-store format v5 → v6).
//!
//! The guarded posture itself is unchanged: the first real account
//! still closes the server to everyone, which is what it is for.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut ty = [0u8; 1];
    s.read_exact(&mut ty).expect("pg type byte");
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("pg length");
    let body_len = u32::from_be_bytes(len).saturating_sub(4) as usize;
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty: ty[0], body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

/// The first byte of the server's Authentication reply: 0 is
/// AuthenticationOk (open mode), anything else asks for a credential.
fn auth_request(addr: &str, user: &str) -> u32 {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    let r = read_message(&mut s);
    assert_eq!(r.ty, b'R', "expected an Authentication message");
    u32::from_be_bytes(r.body[..4].try_into().expect("auth code"))
}

fn run_sql(addr: &str, sql: &str) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "postgres");
    let r = read_message(&mut s);
    assert_eq!(r.ty, b'R');
    assert_eq!(
        u32::from_be_bytes(r.body[..4].try_into().unwrap()),
        0,
        "setup needs open mode"
    );
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
    let mut q = Vec::new();
    q.push(b'Q');
    let body_len = (sql.len() + 5) as u32;
    q.extend_from_slice(&body_len.to_be_bytes());
    q.extend_from_slice(sql.as_bytes());
    q.push(0);
    s.write_all(&q).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
}

#[test]
fn round548_a_group_role_does_not_close_the_server() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");

    assert_eq!(auth_request(&pg, "postgres"), 0, "a fresh server is open");

    // A role that cannot log in and declares no password.
    run_sql(&pg, "CREATE ROLE devs NOLOGIN");
    assert_eq!(
        auth_request(&pg, "postgres"),
        0,
        "a NOLOGIN group role must not close the server"
    );

    // And one that CAN log in but still declares no password: nobody
    // could ever authenticate as it, so it closes nothing either.
    run_sql(&pg, "CREATE ROLE bob LOGIN");
    assert_eq!(
        auth_request(&pg, "postgres"),
        0,
        "a passwordless LOGIN role must not close the server"
    );

    // The first REAL account does close it — that is what it is for.
    run_sql(&pg, "CREATE USER alice PASSWORD 'x'");
    assert_ne!(
        auth_request(&pg, "postgres"),
        0,
        "a declared password must arm the guard"
    );
    assert_ne!(auth_request(&pg, "alice"), 0, "…for the account itself too");

    let _ = child.0.kill();
}
