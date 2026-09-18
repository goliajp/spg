//! 9.0.0 — one clock reading per statement, and none of them cached.
//!
//! Three defects of one family, all measured against PostgreSQL 18.6:
//!
//! 1. A view's body is parsed and folded during execution, so it read a
//!    later instant than the statement that named it:
//!    `SELECT (SELECT n FROM v) = now()` answered `f` where PG answers
//!    `t`.
//! 2. `PREPARE p AS SELECT now(); EXECUTE p` answered
//!    `ERROR: function now() does not exist` — the stored body had
//!    reached none of the pre-passes.
//! 3. Both plan caches were keyed on SQL text and held the folded
//!    instant, so a repeated statement answered the SAME microsecond
//!    for the life of the process. Over the extended protocol that was
//!    every clock spelling; over the simple one it was the ten
//!    spellings the wire's own list of clock names did not know
//!    (`localtimestamp`, `localtime`, `statement_timestamp()`, …).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough that two readings of a microsecond clock cannot collide,
/// short enough to keep the pin cheap.
const APART: Duration = Duration::from_millis(250);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-stmtclock-{label}-{nanos}"));
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0admin\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    read_until_ready(&mut s);
    s
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

/// The first column of the first `DataRow`, as text.
fn first_cell(msgs: &[PgMessage], what: &str) -> String {
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{what}: error {}", String::from_utf8_lossy(&e.body));
    }
    let d = msgs
        .iter()
        .find(|m| m.ty == b'D')
        .unwrap_or_else(|| panic!("{what}: no row"));
    let b = &d.body;
    assert!(b.len() >= 6, "{what}: short DataRow");
    let len = i32::from_be_bytes([b[2], b[3], b[4], b[5]]);
    assert!(len > 0, "{what}: NULL cell");
    let n = usize::try_from(len).unwrap();
    String::from_utf8_lossy(&b[6..6 + n]).into_owned()
}

fn simple(s: &mut TcpStream, sql: &str) -> String {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let msgs = read_until_ready(s);
    first_cell(&msgs, sql)
}

fn run_simple(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let msgs = read_until_ready(s);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: error {}", String::from_utf8_lossy(&e.body));
    }
}

/// Parse + Bind + Execute + Sync, the route every driver takes.
fn extended(s: &mut TcpStream, sql: &str) -> String {
    let mut parse = Vec::new();
    parse.push(0); // unnamed statement
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&0_u16.to_be_bytes());
    send_msg(s, b'P', &parse);

    let mut bind = Vec::new();
    bind.push(0); // unnamed portal
    bind.push(0); // unnamed statement
    bind.extend_from_slice(&0_u16.to_be_bytes()); // parameter formats
    bind.extend_from_slice(&0_u16.to_be_bytes()); // parameters
    bind.extend_from_slice(&0_u16.to_be_bytes()); // result formats
    send_msg(s, b'B', &bind);

    let mut exec = Vec::new();
    exec.push(0);
    exec.extend_from_slice(&0_u32.to_be_bytes());
    send_msg(s, b'E', &exec);
    send_msg(s, b'S', &[]);

    let msgs = read_until_ready(s);
    first_cell(&msgs, sql)
}

/// The nine spellings the folder answers with the statement's clock.
/// `current_date` is left out: it is the same answer all day by
/// definition, on both engines.
const SPELLINGS: &[&str] = &[
    "now()",
    "current_timestamp",
    "localtimestamp",
    "statement_timestamp()",
    "clock_timestamp()",
    "transaction_timestamp()",
    "current_time",
    "localtime",
    "sysdate()",
];

#[test]
fn a_views_now_is_the_statements_now() {
    let dir = unique_tmpdir("view");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_simple(&mut s, "CREATE VIEW v_now AS SELECT now() AS n");
    // PG 18.6 answers `t` for both.
    assert_eq!(
        simple(&mut s, "SELECT (SELECT n FROM v_now) = now() AS eq"),
        "t"
    );
    assert_eq!(
        simple(&mut s, "SELECT now() = (SELECT now() FROM v_now) AS eq"),
        "t"
    );
}

#[test]
fn a_prepared_statement_reads_the_clock_of_each_execute() {
    let dir = unique_tmpdir("prep");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_simple(&mut s, "PREPARE pp AS SELECT now() AS n");
    let first = simple(&mut s, "EXECUTE pp");
    std::thread::sleep(APART);
    let second = simple(&mut s, "EXECUTE pp");
    assert_ne!(
        first, second,
        "EXECUTE answered the clock of the PREPARE twice"
    );
}

#[test]
fn a_repeated_statement_reads_the_clock_again() {
    let dir = unique_tmpdir("cache");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    for f in SPELLINGS {
        let sql = format!("SELECT {f} AS t");

        let a = simple(&mut s, &sql);
        std::thread::sleep(APART);
        let b = simple(&mut s, &sql);
        assert_ne!(a, b, "{sql}: frozen on the simple protocol");

        let c = extended(&mut s, &sql);
        std::thread::sleep(APART);
        let d = extended(&mut s, &sql);
        assert_ne!(c, d, "{sql}: frozen on the extended protocol");
    }
}

#[test]
fn a_transactions_now_stands_still_and_its_statements_do_not() {
    let dir = unique_tmpdir("tx");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_simple(&mut s, "BEGIN");
    let xact = simple(&mut s, "SELECT now() AS t");
    let stmt = simple(&mut s, "SELECT statement_timestamp() AS t");
    std::thread::sleep(APART);
    // PG 18.6: `now()` is the BEGIN for the whole block, and
    // `statement_timestamp()` moves with each statement.
    assert_eq!(simple(&mut s, "SELECT now() AS t"), xact);
    assert_ne!(simple(&mut s, "SELECT statement_timestamp() AS t"), stmt);
    run_simple(&mut s, "COMMIT");
}
