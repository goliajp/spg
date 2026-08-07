//! v7.39 (round 219) — cursor command tags on the pgwire. PG tags cursor
//! statements distinctly and psql / drivers branch on them:
//!   DECLARE … CURSOR → `DECLARE CURSOR`
//!   FETCH …          → `FETCH <n>` (n = rows streamed)
//!   MOVE …           → `MOVE <n>`  (n = rows skipped)
//!   CLOSE <name>     → `CLOSE CURSOR`
//!   CLOSE ALL        → `CLOSE CURSOR ALL`
//! Verified with the raw-wire client (startup proto 196608 + 'Q'),
//! reading every CommandComplete ('C') tag byte-for-byte.

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
    let p = std::env::temp_dir().join(format!("spg-cursor-wire-{nanos}"));
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

/// Run one simple query; return (CommandComplete tag, DataRow count).
/// Panics on an ErrorResponse (these tests expect success).
fn exec_tag(s: &mut TcpStream, sql: &str) -> (String, usize) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut tag = String::new();
    let mut rows = 0usize;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => rows += 1,
            b'C' => {
                // Tag is a NUL-terminated cstring.
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                tag = String::from_utf8_lossy(&body[..end]).into_owned();
            }
            b'E' => {
                let msg = String::from_utf8_lossy(&body).into_owned();
                panic!("unexpected ErrorResponse for {sql:?}: {msg}");
            }
            b'Z' => return (tag, rows),
            _ => {}
        }
    }
}

#[test]
fn cursor_command_tags_match_pg() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());

    exec_tag(&mut s, "CREATE TABLE c (id int)");
    exec_tag(&mut s, "INSERT INTO c VALUES (1),(2),(3),(4),(5)");
    assert_eq!(exec_tag(&mut s, "BEGIN").0, "BEGIN");
    assert_eq!(
        exec_tag(
            &mut s,
            "DECLARE cur SCROLL CURSOR FOR SELECT id FROM c ORDER BY id"
        )
        .0,
        "DECLARE CURSOR"
    );
    // FETCH streams DataRows and tags FETCH <n>.
    assert_eq!(
        exec_tag(&mut s, "FETCH 3 FROM cur"),
        ("FETCH 3".to_string(), 3)
    );
    // Past-the-remaining fetch: only 2 left.
    assert_eq!(
        exec_tag(&mut s, "FETCH 5 FROM cur"),
        ("FETCH 2".to_string(), 2)
    );
    // Exhausted: FETCH 0 rows.
    assert_eq!(
        exec_tag(&mut s, "FETCH NEXT FROM cur"),
        ("FETCH 0".to_string(), 0)
    );
    // MOVE reports the skip count, streams nothing.
    assert_eq!(
        exec_tag(&mut s, "MOVE BACKWARD 2 FROM cur"),
        ("MOVE 2".to_string(), 0)
    );
    assert_eq!(exec_tag(&mut s, "CLOSE cur").0, "CLOSE CURSOR");
    assert_eq!(exec_tag(&mut s, "CLOSE ALL").0, "CLOSE CURSOR ALL");
    assert_eq!(exec_tag(&mut s, "COMMIT").0, "COMMIT");
}
