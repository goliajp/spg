//! r832 — SERIALIZABLE detects the conflict snapshot isolation cannot see.
//!
//! `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` was accepted and `SHOW`
//! answered `serializable`, but nothing proved a conflict was ever
//! detected — an isolation level that only reports itself is worse than
//! not offering one, because an application trusts it and stops taking
//! its own locks.
//!
//! Write skew is the shape that separates the two. Each transaction
//! reads what the other is about to write, so neither sees a conflicting
//! ROW; the anomaly is in the read/write dependency between them, which
//! is exactly what snapshot isolation misses and SSI is for.
//!
//! Measured against PG18 over the same interleaving, driven through two
//! psql sessions: T1 commits, T2 aborts with `could not serialize access
//! due to read/write dependencies among transactions`, and the table ends
//! four rows black. SPG answers the same, with 40001 and PG's sentence.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
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

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn error_field(msgs: &[PgMessage], want: u8) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == want {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn col0(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            out.push(String::new());
            continue;
        }
        out.push(String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned());
    }
    out
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = q(s, sql);
    assert_eq!(
        error_field(&msgs, b'C'),
        None,
        "statement failed: {sql} -> {}",
        error_field(&msgs, b'M').unwrap_or_default()
    );
    msgs
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = crate::common::tmp_base().join(format!("spg-e2e-ssi-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

#[test]
fn write_skew_between_two_serializable_transactions_aborts_one() {
    let (_child, addrs) = spawn("skew");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut setup = open(addr);
    ok(&mut setup, "CREATE TABLE mm (id INT, color TEXT)");
    ok(
        &mut setup,
        "INSERT INTO mm VALUES (1,'black'),(2,'black'),(3,'white'),(4,'white')",
    );

    let mut t1 = open(addr);
    let mut t2 = open(addr);
    ok(&mut t1, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    ok(&mut t2, "BEGIN ISOLATION LEVEL SERIALIZABLE");

    // Each reads what the other is about to write. Neither sees a
    // conflicting row, which is why snapshot isolation lets this through.
    assert_eq!(
        col0(&ok(&mut t1, "SELECT count(*) FROM mm WHERE color='black'")),
        vec!["2"]
    );
    assert_eq!(
        col0(&ok(&mut t2, "SELECT count(*) FROM mm WHERE color='white'")),
        vec!["2"]
    );

    ok(&mut t1, "UPDATE mm SET color='black' WHERE color='white'");
    ok(&mut t2, "UPDATE mm SET color='white' WHERE color='black'");

    // First committer wins, as in PG.
    ok(&mut t1, "COMMIT");
    let second = q(&mut t2, "COMMIT");
    assert_eq!(
        error_field(&second, b'C').as_deref(),
        Some("40001"),
        "the second commit closes the dependency cycle and must abort; got: {}",
        error_field(&second, b'M').unwrap_or_else(|| "no error at all".into())
    );
    assert!(
        error_field(&second, b'M')
            .unwrap_or_default()
            .contains("could not serialize access due to read/write dependencies"),
        "PG's own sentence, so a client matching on text sees no difference"
    );

    // And the outcome is the serial one: T1 ran, T2 did not.
    assert_eq!(
        col0(&ok(
            &mut setup,
            "SELECT count(*) FROM mm WHERE color='black'"
        )),
        vec!["4"],
        "T1's write stands and T2's is gone — the same state PG18 ends in"
    );
}

#[test]
fn the_same_interleaving_is_allowed_under_read_committed() {
    // The point of the pin above is that SERIALIZABLE detects something,
    // not that concurrent updates are refused in general. Under the
    // default level the identical interleaving commits both sides — PG
    // does too, which is exactly why the anomaly needs SSI to catch.
    let (_child, addrs) = spawn("rc");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut setup = open(addr);
    ok(&mut setup, "CREATE TABLE mm (id INT, color TEXT)");
    ok(
        &mut setup,
        "INSERT INTO mm VALUES (1,'black'),(2,'black'),(3,'white'),(4,'white')",
    );

    let mut t1 = open(addr);
    let mut t2 = open(addr);
    ok(&mut t1, "BEGIN");
    ok(&mut t2, "BEGIN");
    ok(&mut t1, "SELECT count(*) FROM mm WHERE color='black'");
    ok(&mut t2, "SELECT count(*) FROM mm WHERE color='white'");
    ok(&mut t1, "UPDATE mm SET color='black' WHERE color='white'");
    ok(&mut t1, "COMMIT");
    let second = q(&mut t2, "COMMIT");
    assert_eq!(
        error_field(&second, b'C'),
        None,
        "read committed does not police read/write dependencies: {}",
        error_field(&second, b'M').unwrap_or_default()
    );
}
