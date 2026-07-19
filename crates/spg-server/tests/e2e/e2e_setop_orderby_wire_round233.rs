//! v7.39 (round 233) — the round 232 / 233 set-operation and ORDER BY
//! errors on the pgwire. Round 230 learned the hard way that a SQLSTATE
//! classifier written without watching a real ErrorResponse can be dead
//! code (parse errors and `Unsupported` both short-circuit ahead of the
//! message arms), so these classes are pinned over a raw connection.
//! Live PG18.4 (2026-07-19): 42P10 for the ORDER BY legality rules, 42601
//! for a set-operation arity mismatch, 42804 for two branch column types
//! with no common type, and 22P02 for an untyped literal that will not
//! convert to the other branch's type.

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
    let p = std::env::temp_dir().join(format!("spg-setop-wire-{nanos}"));
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

fn seeded() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec_error(&mut s, "CREATE TABLE t (a int, b text)"), None);
    assert_eq!(
        exec_error(&mut s, "INSERT INTO t VALUES (1,'x'),(2,'y'),(3,NULL)"),
        None
    );
    (raw, s)
}

#[test]
fn order_by_legality_errors_are_42p10_over_the_wire() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    for (sql, want) in [
        ("SELECT a FROM t ORDER BY 2", "is not in select list"),
        (
            "SELECT DISTINCT a FROM t ORDER BY b",
            "must appear in select list",
        ),
        (
            "SELECT DISTINCT ON (a) a, b FROM t ORDER BY b",
            "must match initial ORDER BY expressions",
        ),
    ] {
        let (code, msg) = exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error: {sql}"));
        assert_eq!(code, "42P10", "{sql} → {msg}");
        assert!(msg.contains(want), "{sql} → {msg}");
    }
}

#[test]
fn set_operation_errors_carry_pgs_classes() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    // Arity: 42601 SYNTAX_ERROR.
    let (code, msg) = exec_error(&mut s, "SELECT a FROM t UNION SELECT a, b FROM t").unwrap();
    assert_eq!(code, "42601", "{msg}");
    assert!(
        msg.contains("each UNION query must have the same number of columns"),
        "{msg}"
    );
    // No common type: 42804 DATATYPE_MISMATCH.
    for (sql, want) in [
        (
            "SELECT a, b FROM t UNION SELECT b, a FROM t",
            "UNION types integer and text cannot be matched",
        ),
        (
            "SELECT a FROM t EXCEPT SELECT b FROM t",
            "EXCEPT types integer and text cannot be matched",
        ),
    ] {
        let (code, msg) = exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error: {sql}"));
        assert_eq!(code, "42804", "{sql} → {msg}");
        assert!(msg.contains(want), "{sql} → {msg}");
    }
    // An untyped literal that will not convert is an input-syntax error on
    // the value, not a type mismatch — PG's 22P02.
    let (code, msg) = exec_error(&mut s, "SELECT a FROM t UNION SELECT 'zz'").unwrap();
    assert_eq!(code, "22P02", "{msg}");
    assert!(
        msg.contains("invalid input syntax for type integer: \"zz\""),
        "{msg}"
    );
}

/// v7.39 (round 235) — the JSON modification (round 234) and jsonpath
/// strict-mode (round 235) refusals over the wire. PG gives each jsonpath
/// failure its own SQL/JSON class rather than one shared code, which a
/// client branching on the SQLSTATE would notice.
#[test]
fn json_refusals_carry_pgs_sqlstates() {
    let (raw, mut s) = {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&unique_db())
            .with_pgwire()
            .spawn();
        let s = pg_connect(addrs.pgwire.as_ref().unwrap());
        (raw, s)
    };
    let _guard = common::ChildGuard(raw);
    for (sql, code, want) in [
        // round 234 — the modification family, all 22023.
        (
            "SELECT '\"str\"'::jsonb - 'a'",
            "22023",
            "cannot delete from scalar",
        ),
        (
            "SELECT '{\"a\":1}'::jsonb - 0",
            "22023",
            "cannot delete from object using integer index",
        ),
        (
            "SELECT jsonb_set('\"str\"','{a}','9')",
            "22023",
            "cannot set path in scalar",
        ),
        // round 235 — jsonpath strict mode, three different classes.
        (
            "SELECT jsonb_path_query('{\"a\":1}','strict $.b')",
            "2203A",
            "JSON object does not contain key",
        ),
        (
            "SELECT jsonb_path_query('1','strict $.a')",
            "2203A",
            "jsonpath member accessor can only be applied to an object",
        ),
        (
            "SELECT jsonb_path_query('[1,2]','strict $[5]')",
            "22033",
            "jsonpath array subscript is out of bounds",
        ),
        (
            "SELECT jsonb_path_query('1','strict $[*]')",
            "22039",
            "jsonpath wildcard array accessor can only be applied to an array",
        ),
    ] {
        let (got_code, msg) =
            exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error for {sql}"));
        assert_eq!(got_code, code, "{sql} → {msg}");
        assert!(msg.contains(want), "{sql} → {msg}");
    }
}

/// v7.39 (round 240) — the round 239 row-count clause errors over the
/// wire. They are raised by the parser, so without the pre-classification
/// they would all have fallen into the Parse→42601 short-circuit; PG uses
/// 2201W / 2201X / 22P02 here.
#[test]
fn row_count_clause_errors_carry_pgs_sqlstates() {
    let (raw, mut s) = {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&unique_db())
            .with_pgwire()
            .spawn();
        let s = pg_connect(addrs.pgwire.as_ref().unwrap());
        (raw, s)
    };
    let _guard = common::ChildGuard(raw);
    for (sql, code, want) in [
        ("SELECT 1 LIMIT -1", "2201W", "LIMIT must not be negative"),
        ("SELECT 1 FETCH FIRST -1 ROWS ONLY", "2201W", "LIMIT must not be negative"),
        ("SELECT 1 OFFSET -1", "2201X", "OFFSET must not be negative"),
        (
            "SELECT 1 LIMIT 'a'",
            "22P02",
            "invalid input syntax for type bigint: \"a\"",
        ),
    ] {
        let (got_code, msg) =
            exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error for {sql}"));
        assert_eq!(got_code, code, "{sql} → {msg}");
        assert!(msg.contains(want), "{sql} → {msg}");
    }
}

/// v7.39 (round 243) — round 242's grouping error over the wire (42803
/// GROUPING_ERROR, parser-raised so classified ahead of the Parse→42601
/// short-circuit).
#[test]
fn grouping_error_carries_42803() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    let (code, msg) = exec_error(&mut s, "SELECT grouping(a) FROM t").unwrap();
    assert_eq!(code, "42803", "{msg}");
    assert!(
        msg.contains("arguments to GROUPING must be grouping expressions"),
        "{msg}"
    );
}

/// v7.39 (round 245) — round 244's sequence-range errors over the wire.
#[test]
fn sequence_range_errors_carry_pgs_classes() {
    let (raw, mut s) = {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&unique_db())
            .with_pgwire()
            .spawn();
        let s = pg_connect(addrs.pgwire.as_ref().unwrap());
        (raw, s)
    };
    let _guard = common::ChildGuard(raw);
    assert_eq!(exec_error(&mut s, "CREATE SEQUENCE wsq MAXVALUE 6"), None);
    for (sql, code, want) in [
        (
            "SELECT setval('wsq', 99)",
            "22003",
            "is out of bounds for sequence",
        ),
        (
            "CREATE SEQUENCE wbad MINVALUE 5 START 3",
            "22023",
            "START value (3) cannot be less than MINVALUE (5)",
        ),
        (
            "SELECT nextval('wnope')",
            "42P01",
            "relation \"wnope\" does not exist",
        ),
    ] {
        let (got_code, msg) =
            exec_error(&mut s, sql).unwrap_or_else(|| panic!("no error for {sql}"));
        assert_eq!(got_code, code, "{sql} → {msg}");
        assert!(msg.contains(want), "{sql} → {msg}");
    }
}
