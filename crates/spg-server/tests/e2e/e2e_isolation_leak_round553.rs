//! v7.39 (round 553) — one connection's isolation level was every
//! connection's.
//!
//! Round 552 found `Engine::current_isolation_level` giving way under
//! it — the level a transaction ran at was reset by another
//! connection's COMMIT — and worked around it by putting the level on
//! the TRANSACTION. That fixed the write-skew check and left the leak
//! itself standing. Measured over pgwire against PG18, it runs both
//! ways:
//!
//!     A: BEGIN ISOLATION LEVEL SERIALIZABLE
//!     B: BEGIN;  SHOW transaction_isolation
//!        PG18  read committed        SPG  serializable
//!     B: COMMIT
//!     A: SHOW transaction_isolation
//!        PG18  serializable          SPG  read committed
//!
//! So a transaction that asked for SERIALIZABLE ran at READ COMMITTED,
//! and one that asked for nothing ran at SERIALIZABLE, purely because
//! another connection happened to be busy.
//!
//! The level joins the rest of the per-connection state in the session
//! bag — the place r306's comment says every piece of it belongs, "so
//! this one never gets a process-wide version to regress from", after
//! rounds 277, 279 and 283 each had to unpick one.
//!
//! Every expectation below is a PG18 reading.

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

fn connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            return s;
        }
    }
}

/// Run one simple query and return its first row's first field.
fn query(s: &mut TcpStream, sql: &str) -> String {
    let mut q = vec![b'Q'];
    q.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
    q.extend_from_slice(sql.as_bytes());
    q.push(0);
    s.write_all(&q).unwrap();
    let mut answer = String::new();
    loop {
        let m = read_message(s);
        match m.ty {
            // DataRow: [u16 fields][i32 len][bytes]…
            b'D' if answer.is_empty() && m.body.len() > 6 => {
                let len = i32::from_be_bytes(m.body[2..6].try_into().unwrap());
                if len > 0 {
                    let end = 6 + len as usize;
                    answer = String::from_utf8_lossy(&m.body[6..end]).into_owned();
                }
            }
            b'Z' => return answer,
            _ => {}
        }
    }
}

#[test]
fn round553_isolation_level_is_per_connection() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");

    let mut a = connect(&pg);
    let mut b = connect(&pg);

    query(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(query(&mut a, "SHOW transaction_isolation"), "serializable");

    // B asked for nothing and must get PG's default, not A's level.
    query(&mut b, "BEGIN");
    assert_eq!(
        query(&mut b, "SHOW transaction_isolation"),
        "read committed",
        "a plain BEGIN must not inherit another connection's level"
    );
    query(&mut b, "COMMIT");

    // And B's COMMIT must not have reset A's.
    assert_eq!(
        query(&mut a, "SHOW transaction_isolation"),
        "serializable",
        "another connection's COMMIT must not change this one's level"
    );
    query(&mut a, "COMMIT");

    // Back at the session default once its own block ends.
    assert_eq!(query(&mut a, "SHOW transaction_isolation"), "read committed");

    let _ = child.0.kill();
}
