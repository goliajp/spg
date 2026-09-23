//! 9.0.4 — `max_connections` and the idle timeout apply to the port
//! clients actually use.
//!
//! Both were claimed and set on the NATIVE accept loop only, and every
//! PostgreSQL client arrives on the other one. Measured on the candidate
//! image before the fix, against PostgreSQL 18.6 with the same limits:
//!
//! ```text
//!   SPG_MAX_CONNECTIONS=5, nine clients at once
//!     SPG: all nine served          PG: FATAL: sorry, too many clients already
//!   SPG_IDLE_TIMEOUT_SEC=3, idle six seconds, then query
//!     SPG: answered                 PG: FATAL: terminating connection due to
//!                                        idle-session timeout
//! ```
//!
//! An operator's cap on a connection pool is what keeps the process from
//! running out of memory, and it was doing nothing.
//!
//! The refusal is delivered where PostgreSQL delivers it — after the
//! startup message. Sent any earlier, a client reads it as a broken SSL
//! exchange (`server sent an error response during SSL exchange`), which
//! names the wrong thing entirely; that is what the first cut of this fix
//! did.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Connect and send the startup message, returning what came back:
/// `Ok(stream)` once ReadyForQuery arrives, `Err(sqlstate: message)` on
/// an ErrorResponse.
fn try_open(addr: &str) -> Result<TcpStream, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608u32.to_be_bytes());
    for (k, v) in [("user", "postgres"), ("database", "probe")] {
        startup.extend_from_slice(k.as_bytes());
        startup.push(0);
        startup.extend_from_slice(v.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut framed = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&startup);
    s.write_all(&framed).map_err(|e| format!("write: {e}"))?;
    loop {
        let mut header = [0u8; 5];
        s.read_exact(&mut header)
            .map_err(|e| format!("read: {e}"))?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        if !body.is_empty() {
            s.read_exact(&mut body).map_err(|e| format!("body: {e}"))?;
        }
        match header[0] {
            b'Z' => return Ok(s),
            b'E' => return Err(error_text(&body)),
            _ => {}
        }
    }
}

/// `SQLSTATE: message` out of an ErrorResponse body.
fn error_text(body: &[u8]) -> String {
    let (mut code, mut msg) = (String::new(), String::new());
    let mut at = 0;
    while at < body.len() && body[at] != 0 {
        let tag = body[at];
        at += 1;
        let end = body[at..]
            .iter()
            .position(|b| *b == 0)
            .map_or(body.len(), |p| at + p);
        let val = String::from_utf8_lossy(&body[at..end]).into_owned();
        at = end + 1;
        match tag {
            b'C' => code = val,
            b'M' => msg = val,
            _ => {}
        }
    }
    format!("{code}: {msg}")
}

fn server(name: &str, cap: &str, idle: &str) -> (common::ChildGuard, String) {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .env("SPG_MAX_CONNECTIONS", cap)
        .env("SPG_IDLE_TIMEOUT_SEC", idle)
        .spawn();
    (common::ChildGuard(raw), addrs.pgwire.unwrap())
}

#[test]
fn the_connection_cap_applies_to_the_postgresql_port() {
    // The cap counts every connection the server holds, and the harness
    // has none of its own — so the first N opens have to succeed and
    // N + 1 has to be refused.
    let (_child, addr) = server("connlimit904", "4", "0");
    let mut held = Vec::new();
    for i in 0..3 {
        match try_open(&addr) {
            Ok(s) => held.push(s),
            Err(e) => panic!("connection {i} of the first three was refused: {e}"),
        }
    }
    // One more than the cap. PostgreSQL 18.6's own words.
    let mut refused = None;
    for _ in 0..4 {
        match try_open(&addr) {
            Ok(s) => held.push(s),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert_eq!(
        refused.as_deref(),
        Some("53300: sorry, too many clients already"),
        "the cap let {} connections through",
        held.len(),
    );

    // Closing one frees its slot: the cap is a limit, not a lifetime
    // budget.
    held.pop();
    std::thread::sleep(Duration::from_millis(500));
    try_open(&addr).expect("a closed connection's slot is reusable");
}

#[test]
fn an_idle_session_is_closed_and_told_so() {
    let (_child, addr) = server("connidle904", "0", "2");
    let mut s = try_open(&addr).expect("connect");
    // Say nothing for longer than the timeout.
    std::thread::sleep(Duration::from_secs(4));
    let mut header = [0u8; 5];
    let n = s.read(&mut header).unwrap_or(0);
    assert!(n >= 5, "the server said nothing before closing ({n} bytes)");
    assert_eq!(
        header[0], b'E',
        "expected an ErrorResponse, got {:?}",
        header[0]
    );
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    s.read_exact(&mut body).expect("error body");
    assert_eq!(
        error_text(&body),
        "57P05: terminating connection due to idle-session timeout",
        "PostgreSQL 18.6's own wording",
    );
}
