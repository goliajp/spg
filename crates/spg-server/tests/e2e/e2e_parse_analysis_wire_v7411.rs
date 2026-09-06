//! v7.40.11 — Parse must refuse the statements PostgreSQL refuses.
//!
//! Reported against 7.40.9 (§3.4). `describe_prepared` answers a SHAPE
//! and had no error channel, so Parse always succeeded:
//!
//! ```text
//!                                    PG 18.6                    SPG 7.40.10
//!   SELECT * FROM no_such_table      ERROR 42P01 at Parse       ParseComplete
//!   SELECT nosuchcol FROM pg_class   ERROR 42703 at Parse       ParseComplete,
//!                                                               then nosuchcol|text
//! ```
//!
//! The second is the one that costs something: the server invents a
//! column and gives it a type. A driver believes the Parse, binds
//! against a shape that does not exist, and fails at Execute one round
//! trip later; a tool that only ever Describes — a schema browser, a
//! query builder — is never corrected at all.
//!
//! The SQLSTATE is part of the fix. Parse reported every failure as
//! 42601 (syntax error), which tells a driver the statement could never
//! be valid; PG says 42P01 for a missing relation and 42703 for a
//! missing column, and a driver's retry-after-migration logic reads
//! that number.
//!
//! This is the wire half. The analysis itself is pinned in
//! `spg-engine`'s `e2e_describe_validates_v7411`, against `PREPARE`,
//! which is the same entry point.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-parse-analysis-{nanos}"));
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

/// What Parse + Describe(statement) + Sync produced.
enum Parsed {
    /// ParseComplete, then the column names the server described.
    Described(Vec<String>),
    /// ErrorResponse: (SQLSTATE, message).
    Refused(String, String),
}

fn parse_and_describe(s: &mut TcpStream, sql: &str) -> Parsed {
    let mut out: Vec<u8> = Vec::new();
    let mut p: Vec<u8> = vec![0];
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);

    // Describe the unnamed STATEMENT, which is what a driver does to
    // learn the shape before it binds.
    let d: Vec<u8> = vec![b'S', 0];
    out.push(b'D');
    out.extend_from_slice(&((d.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&d);

    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();

    let mut columns: Vec<String> = Vec::new();
    let mut refused: Option<(String, String)> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'T' => {
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut pos = 2;
                for _ in 0..n {
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    columns.push(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    pos = end + 1 + 18;
                }
            }
            b'E' => {
                let mut msg = String::new();
                let mut code = String::new();
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    let v = String::from_utf8_lossy(&body[pos..end]).into_owned();
                    match tag {
                        b'M' => msg = v,
                        b'C' => code = v,
                        _ => {}
                    }
                    pos = end + 1;
                }
                refused = Some((code, msg));
            }
            b'Z' => break,
            _ => {}
        }
    }
    match refused {
        Some((code, msg)) => Parsed::Refused(code, msg),
        None => Parsed::Described(columns),
    }
}

fn server() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let s = pg_connect(addrs.pgwire.as_ref().unwrap());
    (raw, s)
}

fn refusal(s: &mut TcpStream, sql: &str) -> (String, String) {
    match parse_and_describe(s, sql) {
        Parsed::Refused(code, msg) => (code, msg),
        Parsed::Described(cols) => {
            panic!("{sql}: Parse succeeded and described {cols:?}; PG 18.6 refuses it")
        }
    }
}

#[test]
fn parse_refuses_what_pg_refuses_with_pgs_sqlstate() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);

    // Setup through the same connection.
    for sql in [
        "CREATE TABLE pw (a INT, b TEXT)",
        "INSERT INTO pw VALUES (1,'x')",
    ] {
        match parse_and_describe(&mut s, sql) {
            Parsed::Described(_) => {}
            Parsed::Refused(c, m) => panic!("setup {sql}: {c} {m}"),
        }
        // Bindless statements still need running; a Parse alone does
        // not create the table. Use the simple query protocol.
        let mut q: Vec<u8> = Vec::new();
        q.push(b'Q');
        let mut b = sql.as_bytes().to_vec();
        b.push(0);
        q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
        q.extend_from_slice(&b);
        s.write_all(&q).unwrap();
        loop {
            if pg_msg(&mut s).0 == b'Z' {
                break;
            }
        }
    }

    // (sql, SQLSTATE, message fragment) — every expectation measured on
    // PostgreSQL 18.6.
    let cases: &[(&str, &str, &str)] = &[
        (
            "SELECT * FROM no_such_table",
            "42P01",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "SELECT nosuchcol FROM pg_class",
            "42703",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "SELECT nosuchcol FROM pw",
            "42703",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "SELECT * FROM pw WHERE nosuchcol = 1",
            "42703",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "SELECT * FROM (SELECT * FROM no_such_table) z",
            "42P01",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "INSERT INTO no_such_table VALUES (1)",
            "42P01",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "DELETE FROM pw WHERE nosuchcol = 1",
            "42703",
            "column \"nosuchcol\" does not exist",
        ),
    ];
    for (sql, code, fragment) in cases {
        let (got_code, got_msg) = refusal(&mut s, sql);
        assert_eq!(got_code, *code, "{sql}: SQLSTATE ({got_msg})");
        assert!(got_msg.contains(fragment), "{sql}: {got_msg}");
    }
}

/// And the half that must keep working: a valid statement still Parses
/// and still describes its columns. A Parse-time check that refuses one
/// of these is worse than the defect it fixes.
#[test]
fn valid_statements_still_parse_and_describe() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);

    let mut q: Vec<u8> = Vec::new();
    let mut b = b"CREATE TABLE pw2 (a INT, b TEXT)".to_vec();
    b.push(0);
    q.push(b'Q');
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }

    let cases: &[(&str, &[&str])] = &[
        ("SELECT a, b FROM pw2", &["a", "b"]),
        ("SELECT a FROM pw2 WHERE a = $1", &["a"]),
        ("SELECT relname FROM pg_class", &["relname"]),
        ("SELECT * FROM generate_series(1,3) g", &["g"]),
        ("WITH c AS (SELECT 1 AS x) SELECT x FROM c", &["x"]),
        ("SELECT count(*) FROM pw2", &["count"]),
    ];
    for (sql, cols) in cases {
        match parse_and_describe(&mut s, sql) {
            Parsed::Described(got) => assert_eq!(
                got,
                cols.iter().map(|c| (*c).to_string()).collect::<Vec<_>>(),
                "{sql}"
            ),
            Parsed::Refused(c, m) => panic!("{sql}: refused {c} {m}; PG 18.6 parses it"),
        }
    }
}
