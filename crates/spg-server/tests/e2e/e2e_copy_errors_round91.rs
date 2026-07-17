//! v7.39 (read01 round 91) — COPY FROM correctness over the wire.
//!
//! A differential sweep of the COPY family found three real bugs:
//!   * `WITH (NULL 'token')` was ignored — the literal token stayed in the
//!     column instead of decoding to NULL;
//!   * a row with fewer values than columns was fed straight into an INSERT,
//!     which quietly filled the trailing columns with NULL (silent data loss);
//!     PG rejects it with `missing data for column "X"` (22P04);
//!   * after such an error mid-COPY the server sent ReadyForQuery while the
//!     client was still streaming CopyData, desyncing the protocol — it must
//!     drain the client's remaining frames up to CopyDone first.
//!
//! (`COPY (query) TO STDOUT` remains a separate parse gap, deferred.)

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
    let p = std::env::temp_dir().join(format!("spg-e2e-copyerr-{label}-{nanos}"));
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

fn error_sqlstate(body: &[u8]) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let code = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if code == b'C' {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        if code == b'M' {
            // keep scanning; message captured elsewhere
        }
        p = end + 1;
    }
    None
}

fn error_message(body: &[u8]) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let code = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if code == b'M' {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        p = end + 1;
    }
    None
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn exec(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let _ = read_until_ready(s);
}

fn scalar(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    if len < 0 {
        return "NULL".to_string();
    }
    std::str::from_utf8(&dr.body[6..6 + len as usize])
        .unwrap()
        .to_string()
}

#[test]
fn null_option_decodes_the_token_to_null() {
    let dir = unique_tmpdir("null");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec(&mut s, "CREATE TABLE t (id int, name text)");
    send_query(
        &mut s,
        "COPY t FROM STDIN WITH (FORMAT csv, NULL 'NULLTOKEN')",
    );
    assert_eq!(read_message(&mut s).ty, b'G');
    send_msg(&mut s, b'd', b"1,NULLTOKEN\n");
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);

    // The token decoded to NULL, not the literal "NULLTOKEN".
    assert_eq!(
        scalar(
            &mut s,
            "SELECT coalesce(name, 'ISNULL') FROM t WHERE id = 1"
        ),
        "ISNULL"
    );
}

#[test]
fn short_row_is_rejected_and_protocol_stays_in_sync() {
    let dir = unique_tmpdir("short");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec(&mut s, "CREATE TABLE t (a int, b int, c int)");
    send_query(&mut s, "COPY t FROM STDIN");
    assert_eq!(read_message(&mut s).ty, b'G');
    // Two values for a three-column table.
    send_msg(&mut s, b'd', b"1\t2\n");
    send_msg(&mut s, b'c', &[]);
    let msgs = read_until_ready(&mut s);

    // The server rejected it (rather than silently NULL-filling column c)…
    let e = msgs.iter().find(|m| m.ty == b'E').expect("ErrorResponse");
    assert_eq!(error_sqlstate(&e.body).as_deref(), Some("22P04"));
    assert!(
        error_message(&e.body)
            .unwrap_or_default()
            .contains("missing data for column \"c\""),
    );
    // …and the protocol is still usable: the connection answers a normal query.
    assert_eq!(scalar(&mut s, "SELECT count(*)::text FROM t"), "0");
}

#[test]
fn well_formed_rows_still_load() {
    let dir = unique_tmpdir("ok");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec(&mut s, "CREATE TABLE t (a int, b int, c int)");
    send_query(&mut s, "COPY t FROM STDIN");
    assert_eq!(read_message(&mut s).ty, b'G');
    send_msg(&mut s, b'd', b"1\t2\t3\n4\t5\t6\n");
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);
    assert_eq!(scalar(&mut s, "SELECT count(*)::text FROM t"), "2");

    // A column-list COPY expects exactly the listed count.
    send_query(&mut s, "COPY t (a, b) FROM STDIN");
    assert_eq!(read_message(&mut s).ty, b'G');
    send_msg(&mut s, b'd', b"7\t8\n");
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);
    assert_eq!(
        scalar(&mut s, "SELECT coalesce(c::text,'NULL') FROM t WHERE a = 7"),
        "NULL"
    );
}
