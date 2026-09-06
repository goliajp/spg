//! v7.40.10 — every FROM-item kind, through the extended protocol.
//!
//! A `TableRef` carries seven slots that produce rows, and three
//! separate passes each knew a different subset of them. In one day:
//!
//! ```text
//!   7.40.8  the parameter walk knew 1 of 7    unnest($1) → live 500
//!   7.40.9  describe knew 2 of 7              generate_series → protocol error
//!   7.40.10 the parameter walk still knew 3   jsonb_each_text($1), ROWS FROM,
//!           and describe still knew 2         json_table — all three
//! ```
//!
//! Fixing them one at a time is what produced that sequence. The
//! traversal is now written once, next to the type
//! (`TableRef::try_for_each_slot_mut`, a total destructure so a new
//! field is a compile error), and this file is the behavioural half of
//! the same guard: for EVERY kind, ask through a real
//! Parse/Bind/Describe/Execute and assert the client's own invariant —
//! **a RowDescription before any DataRow** — plus the columns it names.
//!
//! Measured on the published 7.40.9 image before the fix, with literal
//! arguments so no parameter is involved:
//!
//! ```text
//!   jsonb_each_text('{"a":1}'::jsonb)   D message without prior T
//!   ROWS FROM (generate_series(1,3))    D message without prior T
//!   json_table('[1,2]'::jsonb, …)       D message without prior T
//! ```
//!
//! Which is what every driver that prepares statements gets.
//!
//! **The third pass, found by this very file and fixed with it.** The
//! streaming wire executor produced rows for four of the nine kinds and
//! read the rest as table names:
//!
//! ```text
//!   SELECT * FROM jsonb_each_text('{"a":1}'::jsonb)
//!     relation "jsonb_each_text" does not exist
//!   SELECT * FROM ROWS FROM (generate_series(1,3))
//!     relation "rows" does not exist
//!   SELECT * FROM json_table('[1,2]'::jsonb, …) jt
//!     relation "jt" does not exist
//! ```
//!
//! `count(*)` over the same items answered, because the aggregate gate
//! sends those to the materialising executor. So did the simple query
//! protocol, and so did all three of the embedded engine's own entry
//! points — which is what made it wire-only and invisible. Its guard
//! named four of seven slots; it now asks `TableRef::kind()` and
//! matches on every variant with no wildcard arm.

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
    let p = common::tmp_base().join(format!("spg-from-item-{nanos}"));
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

/// What one extended-protocol round trip produced: the column names the
/// server described, and the first field of every row it then sent.
struct Described {
    columns: Vec<String>,
    rows: Vec<String>,
}

/// Parse, Bind (text parameters), Describe the PORTAL, Execute, Sync.
///
/// The Describe is the point. A client will not accept a DataRow it has
/// no RowDescription for, and that is asserted here rather than assumed.
fn describe_and_run(s: &mut TcpStream, sql: &str, params: &[&str]) -> Described {
    let mut out: Vec<u8> = Vec::new();
    let mut p: Vec<u8> = vec![0];
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);

    let mut b: Vec<u8> = vec![0, 0];
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(u16::try_from(params.len()).unwrap()).to_be_bytes());
    for v in params {
        b.extend_from_slice(&(i32::try_from(v.len()).unwrap()).to_be_bytes());
        b.extend_from_slice(v.as_bytes());
    }
    b.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'B');
    out.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&b);

    let d: Vec<u8> = vec![b'P', 0];
    out.push(b'D');
    out.extend_from_slice(&((d.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&d);

    let mut e: Vec<u8> = vec![0];
    e.extend_from_slice(&0u32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((e.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&e);

    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();

    let mut got = Described {
        columns: Vec::new(),
        rows: Vec::new(),
    };
    let mut saw_t = false;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'T' => {
                saw_t = true;
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut pos = 2;
                for _ in 0..n {
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    got.columns
                        .push(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    // name, then 18 bytes of oids/lengths/format.
                    pos = end + 1 + 18;
                }
            }
            b'D' => {
                assert!(
                    saw_t,
                    "{sql}: a DataRow before any RowDescription — no client accepts this"
                );
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                got.rows.push(if len < 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned()
                });
            }
            b'E' => {
                let mut msg = String::new();
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if tag == b'M' {
                        msg = String::from_utf8_lossy(&body[pos..end]).into_owned();
                    }
                    pos = end + 1;
                }
                panic!("{sql}: {msg}");
            }
            b'Z' => return got,
            _ => {}
        }
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

/// Every FROM-item kind, with literal arguments. This is the half that
/// has nothing to do with parameters: a Describe that answers no
/// columns is a protocol error on its own.
#[test]
fn every_from_item_kind_describes_its_columns() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);

    // (sql, expected column names, expected row count)
    let cases: &[(&str, &[&str], usize)] = &[
        ("SELECT * FROM unnest(ARRAY[1,2,3]) u", &["u"], 3),
        ("SELECT * FROM generate_series(1,3) g", &["g"], 3),
        (
            "SELECT * FROM jsonb_each_text(cast('{\"a\":1}' as jsonb))",
            &["key", "value"],
            1,
        ),
        (
            "SELECT * FROM ROWS FROM (generate_series(1,3))",
            &["generate_series"],
            3,
        ),
        (
            "SELECT * FROM json_table(cast('[1,2]' as jsonb), '$[*]' \
             COLUMNS (v int PATH '$')) jt",
            &["v"],
            2,
        ),
        ("SELECT * FROM string_to_table('a,b', ',') st", &["st"], 2),
    ];
    for (sql, cols, nrows) in cases {
        let got = describe_and_run(&mut s, sql, &[]);
        assert_eq!(
            got.columns,
            cols.iter().map(|c| (*c).to_string()).collect::<Vec<_>>(),
            "{sql}: described columns"
        );
        assert_eq!(got.rows.len(), *nrows, "{sql}: rows");
    }
}

/// And the parameter half: the same kinds with a bound argument. Each
/// of these answered `parameter $1 referenced but only 0 bound by
/// client` at some point in the 7.40 line, one slot at a time.
#[test]
fn every_from_item_kind_receives_a_bound_parameter() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);

    let cases: &[(&str, &[&str], usize)] = &[
        ("SELECT * FROM unnest($1::int[]) u", &["{1,2,3}"], 3),
        (
            "SELECT * FROM generate_series($1::int, $2::int) g",
            &["1", "3"],
            3,
        ),
        (
            "SELECT * FROM jsonb_each_text($1::jsonb)",
            &["{\"a\":1,\"b\":2}"],
            2,
        ),
        (
            "SELECT * FROM ROWS FROM (generate_series($1::int, $2::int))",
            &["1", "3"],
            3,
        ),
        (
            "SELECT * FROM string_to_table($1, $2) st",
            &["a,b,c", ","],
            3,
        ),
    ];
    for (sql, params, nrows) in cases {
        let got = describe_and_run(&mut s, sql, params);
        assert!(
            !got.columns.is_empty(),
            "{sql}: described no columns at all"
        );
        assert_eq!(got.rows.len(), *nrows, "{sql}: rows");
    }
}

/// A FROM item nested in a derived table, which is where the LIMIT
/// parameter was lost in 7.40.9 — the same "which containers does this
/// pass know" question, one level up.
#[test]
fn a_bound_limit_inside_a_derived_table_is_honoured() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);
    let got = describe_and_run(
        &mut s,
        "SELECT * FROM (SELECT g FROM generate_series(1,5) g ORDER BY g LIMIT $1) z",
        &["2"],
    );
    assert_eq!(got.rows, vec!["1".to_string(), "2".to_string()]);
}
