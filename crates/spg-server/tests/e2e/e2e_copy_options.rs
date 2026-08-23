//! v6.4.7 — COPY enhancements: SKIP N, ON_ERROR SET_NULL,
//! FORMAT JSON.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-copy-opts-{label}-{nanos}"));
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

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
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
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn exec_simple(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let _ = read_until_ready(s);
}

fn count_rows(s: &mut TcpStream, sql: &str) -> i64 {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let dr = msgs.iter().find(|m| m.ty == b'D').expect("DataRow");
    let len = i32::from_be_bytes([dr.body[2], dr.body[3], dr.body[4], dr.body[5]]);
    let v = std::str::from_utf8(&dr.body[6..6 + len as usize]).unwrap();
    v.parse().unwrap()
}

#[test]
fn skip_drops_first_data_row() {
    let dir = unique_tmpdir("skip");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );

    // SKIP 1 drops the header row.
    send_query(&mut s, "COPY t FROM STDIN WITH (SKIP 1)");
    let g = read_message(&mut s);
    assert_eq!(g.ty, b'G');
    let payload = "header\theader\n1\talice\n2\tbob\n";
    send_msg(&mut s, b'd', payload.as_bytes());
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);

    assert_eq!(count_rows(&mut s, "SELECT count(*) FROM t"), 2);
}

#[test]
fn on_error_set_null_skips_bad_row() {
    let dir = unique_tmpdir("on-error");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE t (id INT, name TEXT)");

    // The second row has a non-numeric id; with ON_ERROR SET_NULL
    // the bad row is silently skipped and the COPY continues.
    send_query(&mut s, "COPY t FROM STDIN WITH (ON_ERROR SET_NULL)");
    let g = read_message(&mut s);
    assert_eq!(g.ty, b'G');
    let payload = "1\talice\nNOT_A_NUMBER\tbroken\n3\tcarol\n";
    send_msg(&mut s, b'd', payload.as_bytes());
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);

    let n = count_rows(&mut s, "SELECT count(*) FROM t");
    assert_eq!(n, 2, "broken row should be skipped, others land");
}

#[test]
fn format_json_one_row_per_line() {
    let dir = unique_tmpdir("json");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    exec_simple(&mut s, "CREATE TABLE t (id INT, name TEXT)");

    send_query(&mut s, "COPY t FROM STDIN WITH (FORMAT JSON)");
    let g = read_message(&mut s);
    assert_eq!(g.ty, b'G');
    let payload = "{\"id\":1,\"name\":\"alice\"}\n{\"id\":2,\"name\":\"bob\"}\n{\"id\":3,\"name\":\"carol\"}\n";
    send_msg(&mut s, b'd', payload.as_bytes());
    send_msg(&mut s, b'c', &[]);
    let _ = read_until_ready(&mut s);

    let n = count_rows(&mut s, "SELECT count(*) FROM t");
    assert_eq!(n, 3);
}
