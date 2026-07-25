//! read01 round 429 (MySQL differential) — per-failure errno on the MySQL
//! wire.
//!
//! Every error except the two typed connection-control ones answered
//! `1064 (42000)` — MySQL's SYNTAX ERROR — no matter what actually failed.
//! Clients branch on errno: a duplicate key (1062) drives upsert fallbacks,
//! "table already exists" (1050) makes a migration idempotent, "cannot be
//! null" (1048) and "table doesn't exist" (1146) drive their own paths. All
//! of that logic took the wrong branch.
//!
//! The classification is NOT duplicated: pgwire already sorts the engine's
//! errors into PG SQLSTATEs and is tested on it, so the MySQL errno is
//! derived from that single answer — the two protocols disagree on the
//! code's spelling, never on which failure it was.
//!
//! Every (errno, SQLSTATE) pair is copied from a MariaDB 11 run:
//!   duplicate key   1062 23000     unknown column   1054 42S22
//!   NOT NULL        1048 23000     unknown table    1146 42S02
//!   value too long  1406 22001     table exists     1050 42S01
//!   duplicate col   1060 42S21     syntax error     1064 42000

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let p = std::env::temp_dir().join(format!("spg-e2e-mysqlwire-errno-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).expect("read header");
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let seqno = hdr[3];
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("read payload");
    (seqno, payload)
}

fn write_packet(stream: &mut TcpStream, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    let hdr = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seqno];
    stream.write_all(&hdr).expect("write hdr");
    stream.write_all(payload).expect("write payload");
}

fn build_handshake_response(username: &str) -> Vec<u8> {
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

fn auth_open_mode(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _greeting) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_handshake_response("anyone"));
    let (_seqno, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "expected OK after auth, got {:#x}", ok[0]);
    s
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03); // COM_QUERY
    payload.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &payload);
}

fn spawn() -> (common::ChildGuard, String) {
    let dir = unique_tmpdir("svc");
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

/// Run `sql` and require it to succeed (OK packet, not ERR).
fn ok_query(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let (_seq, reply) = read_packet(s);
    assert_ne!(
        reply[0], 0xff,
        "{sql} should have succeeded, got ERR: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

/// Run `sql`, require an ERR packet, and return `(errno, sqlstate)`.
fn err_of(s: &mut TcpStream, sql: &str) -> (u16, String) {
    send_query(s, sql);
    let (_seq, err) = read_packet(s);
    assert_eq!(
        err[0], 0xff,
        "{sql} should have failed, got {:?}",
        String::from_utf8_lossy(&err)
    );
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    // byte 3 is the '#' SQLSTATE marker; 4..9 is the five-char state.
    let state = String::from_utf8_lossy(&err[4..9]).to_string();
    (errno, state)
}

#[test]
fn each_failure_carries_its_own_mariadb_errno() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    ok_query(&mut s, "SET sql_mode='STRICT_TRANS_TABLES'");
    ok_query(
        &mut s,
        "CREATE TABLE t(id INT PRIMARY KEY, a INT NOT NULL, b VARCHAR(5) UNIQUE)",
    );
    ok_query(&mut s, "INSERT INTO t VALUES(1,1,'x')");

    // Duplicate primary key, then duplicate secondary unique.
    assert_eq!(
        err_of(&mut s, "INSERT INTO t VALUES(1,2,'y')"),
        (1062, "23000".to_string())
    );
    assert_eq!(
        err_of(&mut s, "INSERT INTO t VALUES(2,2,'x')"),
        (1062, "23000".to_string())
    );
    // NOT NULL.
    assert_eq!(
        err_of(&mut s, "INSERT INTO t VALUES(3,NULL,'z')"),
        (1048, "23000".to_string())
    );
    // Value too long for VARCHAR(5).
    assert_eq!(
        err_of(&mut s, "INSERT INTO t VALUES(4,1,'toolongvalue')"),
        (1406, "22001".to_string())
    );
    // Unknown column / unknown table.
    assert_eq!(
        err_of(&mut s, "SELECT nope FROM t"),
        (1054, "42S22".to_string())
    );
    assert_eq!(
        err_of(&mut s, "SELECT * FROM nope"),
        (1146, "42S02".to_string())
    );
    // Table already exists.
    assert_eq!(
        err_of(&mut s, "CREATE TABLE t(x INT)"),
        (1050, "42S01".to_string())
    );
    // Duplicate column on ALTER.
    assert_eq!(
        err_of(&mut s, "ALTER TABLE t ADD COLUMN a INT"),
        (1060, "42S21".to_string())
    );
}

/// A real syntax error still answers 1064 / 42000 — the historical default
/// is now reserved for what it actually names.
#[test]
fn syntax_error_still_1064() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(
        err_of(&mut s, "SELECT FROM WHERE"),
        (1064, "42000".to_string())
    );
}

/// A foreign-key violation carries MariaDB's 1452.
#[test]
fn foreign_key_violation_is_1452() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    ok_query(&mut s, "SET sql_mode='STRICT_TRANS_TABLES'");
    ok_query(&mut s, "CREATE TABLE p(id INT PRIMARY KEY)");
    ok_query(
        &mut s,
        "CREATE TABLE c(id INT PRIMARY KEY, pid INT REFERENCES p(id))",
    );
    assert_eq!(
        err_of(&mut s, "INSERT INTO c VALUES(1,99)"),
        (1452, "23000".to_string())
    );
}
