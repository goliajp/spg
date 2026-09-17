//! v7.39 (read01 round 95) — syntax-error position (the ErrorResponse `P`
//! field). PG reports a 1-based character offset for a syntax error; psql
//! renders it as `LINE n: … ^`. SPG's wire never sent `P`, so psql showed the
//! message with no caret. The parser now carries the failing token's byte
//! offset (via new lexer per-token offsets), mapped to a 1-based char
//! position, and the wire attaches it as `P`.
//!
//! Positions locked against live PG 18.4 (its psql caret column).
//!
//! 9.0.0 — and the SEMANTIC errors carry one too, which is what sentori
//! reported as §3.27: `relation "x" does not exist`, an unresolvable
//! column, an ambiguous one, a missing FROM-clause entry. Every position
//! below is PostgreSQL 18.6's, read off its psql caret.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-errpos-{label}-{nanos}"));
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

fn field(body: &[u8], code: u8) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let c = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if c == code {
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

/// Send `sql`, return the (SQLSTATE, position) from its ErrorResponse.
fn err_pos(s: &mut TcpStream, sql: &str) -> (Option<String>, Option<String>) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("no error for {sql}"));
    (field(&e.body, b'C'), field(&e.body, b'P'))
}

#[test]
fn syntax_error_carries_pg_position() {
    let dir = unique_tmpdir("pos");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // Unexpected token: caret sits at the token (PG points at "FORM").
    let (c, p) = err_pos(&mut s, "SELECT * FORM t");
    assert_eq!(c.as_deref(), Some("42601"));
    assert_eq!(p.as_deref(), Some("10"));

    // Unexpected token mid-list: PG points at "3".
    let (_, p) = err_pos(&mut s, "SELECT 1, 2 3, 4");
    assert_eq!(p.as_deref(), Some("13"));

    // End of input: PG points one past the last char.
    let (_, p) = err_pos(&mut s, "SELECT 1 +");
    assert_eq!(p.as_deref(), Some("11"));

    let (_, p) = err_pos(&mut s, "SELECT * FROM t WHERE");
    assert_eq!(p.as_deref(), Some("22"));
}

#[test]
fn valid_statement_has_no_error() {
    let dir = unique_tmpdir("ok");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "SELECT 1");
    let msgs = read_until_ready(&mut s);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "unexpected error for SELECT 1"
    );
}

/// 9.0.0 — a name the engine could not resolve reports WHERE it is, as
/// PostgreSQL does. Each expected position is 18.6's caret column.
#[test]
fn a_name_that_does_not_resolve_carries_pg_position() {
    let dir = unique_tmpdir("sem");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(&mut s, "CREATE TABLE pe1 (id int, name text, n int)");
    read_until_ready(&mut s);
    send_query(&mut s, "CREATE TABLE pe2 (id int, pe1_id int)");
    read_until_ready(&mut s);

    for (sql, code, pos) in [
        // The statement sentori's corpus stopped on.
        (
            "SELECT version FROM _sqlx_migrations ORDER BY version DESC LIMIT 1",
            "42P01",
            "21",
        ),
        ("SELECT nosuch FROM pe1", "42703", "8"),
        // A qualified reference points at its qualifier, where PG's caret sits.
        ("SELECT pe1.nosuch FROM pe1", "42703", "8"),
        ("SELECT x.id FROM pe1", "42P01", "8"),
        ("SELECT id FROM pe1, pe2", "42702", "8"),
        ("UPDATE pe1 SET n = 1 WHERE nosuch = 2", "42703", "28"),
        ("DELETE FROM pe1 WHERE nosuch = 1", "42703", "23"),
        ("INSERT INTO nosuchtable VALUES (1)", "42P01", "13"),
        ("SELECT * FROM pe1 ORDER BY nosuch", "42703", "28"),
        // A target list that is not grouped: PG points at the column.
        ("SELECT name, count(*) FROM pe1", "42803", "8"),
        // SHOW takes no parameter: PG points at the parameter.
        ("SHOW $1", "42601", "6"),
    ] {
        let (c, p) = err_pos(&mut s, sql);
        assert_eq!(c.as_deref(), Some(code), "{sql}");
        assert_eq!(p.as_deref(), Some(pos), "{sql}");
    }
}
