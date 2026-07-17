//! v7.39 (read01 round 94) — `COPY (<query>) TO STDOUT` over the wire.
//!
//! PG lets you export an arbitrary query, not just a table:
//! `COPY (SELECT … ) TO STDOUT [WITH (FORMAT csv, HEADER, DELIMITER, NULL)]`.
//! SPG's wire COPY detector only recognised `COPY <table> …`, so the query
//! form fell through to the SQL parser and errored. It's now streamed the
//! same way the table form is — and, fixed alongside, the `TO STDOUT` path
//! now honours its WITH options (they were silently dropped, so
//! `COPY … TO STDOUT WITH (FORMAT csv)` used to stream plain text).
//!
//! Every CopyData payload below is locked byte-identical against live
//! PG 18.4 running the identical statement.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-copyq-{label}-{nanos}"));
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

/// Concatenate every CopyData ('d') frame's payload — this is exactly the
/// byte stream a psql `\copy` would write out.
fn copy_out(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    // A COPY OUT must open with CopyOutResponse ('H') and close with
    // CopyDone ('c'); assert both so a regression to a normal result set
    // (RowDescription/DataRow) is caught, not silently string-matched.
    assert!(
        msgs.iter().any(|m| m.ty == b'H'),
        "no CopyOutResponse for {sql}"
    );
    assert!(msgs.iter().any(|m| m.ty == b'c'), "no CopyDone for {sql}");
    let mut out = Vec::new();
    for m in &msgs {
        if m.ty == b'd' {
            out.extend_from_slice(&m.body);
        }
    }
    String::from_utf8(out).unwrap()
}

fn seed(s: &mut TcpStream) {
    exec(s, "CREATE TABLE cq (a int, b text)");
    exec(s, "INSERT INTO cq VALUES (1,'x'),(2,'y'),(3,NULL)");
}

#[test]
fn query_form_text_and_null() {
    let dir = unique_tmpdir("text");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    // Plain text: tab-separated, `\N` for NULL, filtered + ordered.
    assert_eq!(
        copy_out(
            &mut s,
            "COPY (SELECT a, b FROM cq WHERE a <> 2 ORDER BY a) TO STDOUT"
        ),
        "1\tx\n3\t\\N\n"
    );
    // An aggregate query is a valid COPY source.
    assert_eq!(
        copy_out(&mut s, "COPY (SELECT count(*) FROM cq) TO STDOUT"),
        "3\n"
    );
}

#[test]
fn query_form_csv_header_delimiter_null() {
    let dir = unique_tmpdir("csv");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    // CSV + HEADER: names row first, NULL is the empty field.
    assert_eq!(
        copy_out(
            &mut s,
            "COPY (SELECT a, b FROM cq ORDER BY a) TO STDOUT WITH (FORMAT csv, HEADER)"
        ),
        "a,b\n1,x\n2,y\n3,\n"
    );
    // Text + custom DELIMITER keeps `\N`.
    assert_eq!(
        copy_out(
            &mut s,
            "COPY (SELECT a, b FROM cq ORDER BY a) TO STDOUT WITH (DELIMITER '|')"
        ),
        "1|x\n2|y\n3|\\N\n"
    );
    // Custom NULL token in text format.
    assert_eq!(
        copy_out(
            &mut s,
            "COPY (SELECT a, b FROM cq WHERE a = 3) TO STDOUT WITH (NULL 'NADA')"
        ),
        "3\tNADA\n"
    );
    // Text + HEADER emits the names row too.
    assert_eq!(
        copy_out(
            &mut s,
            "COPY (SELECT a, b FROM cq ORDER BY a) TO STDOUT WITH (HEADER)"
        ),
        "a\tb\n1\tx\n2\ty\n3\t\\N\n"
    );
}

#[test]
fn table_form_now_honours_options() {
    // Regression-guard for the silent-drop fix: the classic table form must
    // still stream (text default) AND now honour csv+header.
    let dir = unique_tmpdir("table");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    assert_eq!(
        copy_out(&mut s, "COPY cq TO STDOUT"),
        "1\tx\n2\ty\n3\t\\N\n"
    );
    assert_eq!(
        copy_out(&mut s, "COPY cq TO STDOUT WITH (FORMAT csv, HEADER)"),
        "a,b\n1,x\n2,y\n3,\n"
    );
}
