//! v7.39 (round 230) — the r229 window-clause errors on the pgwire. r229
//! pinned the engine messages and wrote the classifier, but never watched
//! an actual ErrorResponse; this closes that. Live-PG18.4 (2026-07-19)
//! answers every window-clause complaint with SQLSTATE 42P20
//! WINDOWING_ERROR, and reserves 42704 for a genuinely missing name —
//! a split worth pinning, since the copy and redefinition wordings also
//! carry `window "w1"` and would otherwise fall into the 42704 arm.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-window-wire-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
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

/// (SQLSTATE, message) of the first ErrorResponse the query raises, or
/// `None` when it succeeded.
fn exec_error(s: &mut TcpStream, sql: &str) -> Option<(String, String)> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut found: Option<(String, String)> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'E' => {
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
                found = Some((code, msg));
            }
            b'Z' => return found,
            _ => {}
        }
    }
}

#[test]
fn window_clause_errors_are_42p20_over_the_wire() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(
        exec_error(&mut s, "CREATE TABLE w (id int, g text, v int)"),
        None
    );
    assert_eq!(
        exec_error(&mut s, "INSERT INTO w VALUES (1,'a',10),(2,'b',20)"),
        None
    );

    for (sql, want_msg) in [
        (
            "SELECT rank() OVER (ORDER BY v) FROM w WHERE rank() OVER (ORDER BY v) = 1",
            "window functions are not allowed in WHERE",
        ),
        (
            "SELECT id FROM w GROUP BY id HAVING row_number() OVER () = 1",
            "window functions are not allowed in HAVING",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING) FROM w",
            "frame starting from following row cannot have preceding rows",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM w",
            "frame start cannot be UNBOUNDED FOLLOWING",
        ),
        (
            "SELECT sum(v) OVER (w1 PARTITION BY g) FROM w WINDOW w1 AS (PARTITION BY g)",
            "cannot override PARTITION BY clause of window \"w1\"",
        ),
        (
            "SELECT sum(v) OVER (w1) FROM w WINDOW w1 AS (ORDER BY v ROWS 2 PRECEDING)",
            "cannot copy window \"w1\" because it has a frame clause",
        ),
        (
            "SELECT sum(v) OVER w1 FROM w WINDOW w1 AS (PARTITION BY g), w1 AS (PARTITION BY g)",
            "window \"w1\" is already defined",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text",
        ),
    ] {
        let (code, msg) = exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error for {sql}"));
        assert_eq!(code, "42P20", "{sql} → {msg}");
        assert!(msg.contains(want_msg), "{sql} → {msg}");
    }
}

#[test]
fn missing_window_name_stays_42704() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec_error(&mut s, "CREATE TABLE w (id int, v int)"), None);
    // PG: an undefined object, not a windowing error.
    let (code, msg) = exec_error(&mut s, "SELECT sum(v) OVER nosuch FROM w").unwrap();
    assert_eq!(code, "42704", "{msg}");
    assert!(msg.contains("window \"nosuch\" does not exist"), "{msg}");
}
