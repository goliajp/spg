//! v7.40.0 — the WORDS of an error on the MySQL wire, not just its number.
//!
//! v7.39 (round 429) gave each failure MySQL's errno, and
//! `e2e_mysqlwire_errno_round429` keeps them. The first run of
//! `xtests/mysqlcorpus` — the MySQL half of the differential corpus,
//! added this version — found that several of those correct numbers
//! still carried PostgreSQL's SENTENCE, and one carried a PostgreSQL
//! `HINT:` line into a MySQL error packet.
//!
//! Every expectation here was read off MySQL 9.7.2 running the same
//! statements:
//!
//! ```text
//!   Duplicate entry '1-x' for key 'dk.PRIMARY'
//!   Duplicate entry '5' for key 'dk.uq_c'
//!   Table 'mc_t' already exists
//!   Unknown table 'spg.nosuchtable'          (DROP; a read says 1146)
//!   Incorrect datetime value: '2020-99-99' for column 'made' at row 1
//! ```

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-mywords-{label}-{pid}-{nanos}"));
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
    let len = u32::try_from(payload.len()).unwrap();
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

fn ok_query(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let (_seq, reply) = read_packet(s);
    assert_ne!(
        reply[0],
        0xff,
        "{sql} should have succeeded, got ERR: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

/// Run `sql`, require an ERR packet, and return `(errno, sqlstate, message)`.
fn err_of(s: &mut TcpStream, sql: &str) -> (u16, String, String) {
    send_query(s, sql);
    let (_seq, err) = read_packet(s);
    assert_eq!(
        err[0],
        0xff,
        "{sql} should have failed, got {:?}",
        String::from_utf8_lossy(&err)
    );
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    let state = String::from_utf8_lossy(&err[4..9]).to_string();
    let msg = String::from_utf8_lossy(&err[9..]).to_string();
    (errno, state, msg)
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

#[test]
fn a_duplicate_key_is_worded_the_way_mysql_words_it() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    ok_query(
        &mut s,
        "CREATE TABLE dk (a INT, b VARCHAR(8), c INT, PRIMARY KEY (a,b), \
         CONSTRAINT uq_c UNIQUE (c))",
    );
    ok_query(&mut s, "INSERT INTO dk VALUES (1,'x',5)");

    // A composite primary key: MySQL names the index PRIMARY and joins
    // the values with `-`.
    assert_eq!(
        err_of(&mut s, "INSERT INTO dk VALUES (1,'x',6)"),
        (
            1062,
            "23000".to_string(),
            "Duplicate entry '1-x' for key 'dk.PRIMARY'".to_string()
        )
    );
    // A named UNIQUE keeps its own name.
    assert_eq!(
        err_of(&mut s, "INSERT INTO dk VALUES (2,'y',5)"),
        (
            1062,
            "23000".to_string(),
            "Duplicate entry '5' for key 'dk.uq_c'".to_string()
        )
    );
}

#[test]
fn a_table_that_exists_and_one_that_does_not() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    ok_query(&mut s, "CREATE TABLE mc_t (id INT)");
    assert_eq!(
        err_of(&mut s, "CREATE TABLE mc_t (x INT)"),
        (
            1050,
            "42S01".to_string(),
            "Table 'mc_t' already exists".to_string()
        )
    );
    // MySQL uses a DIFFERENT number for a DROP than for a read: 1051
    // `Unknown table` against 1146 `Table … doesn't exist`, both 42S02.
    let (errno, state, msg) = err_of(&mut s, "DROP TABLE nosuchtable");
    assert_eq!((errno, state.as_str()), (1051, "42S02"));
    assert!(
        msg.starts_with("Unknown table '") && msg.ends_with(".nosuchtable'"),
        "DROP of a missing table must use MySQL's words, got {msg:?}"
    );
    let (errno, state, msg) = err_of(&mut s, "SELECT * FROM nosuchtable");
    assert_eq!((errno, state.as_str()), (1146, "42S02"));
    assert!(
        msg.contains("doesn't exist"),
        "a READ of a missing table keeps MySQL's other sentence, got {msg:?}"
    );
}

/// PostgreSQL appends `HINT:` on its own line. MySQL has no such thing,
/// and SPG 7.39.13 sent one inside a MySQL error packet — a second line
/// naming a PostgreSQL GUC.
#[test]
fn no_postgres_hint_line_reaches_the_mysql_wire() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    ok_query(&mut s, "CREATE TABLE dt (made DATETIME)");
    let (errno, state, msg) = err_of(&mut s, "INSERT INTO dt VALUES ('2020-99-99')");
    assert_eq!((errno, state.as_str()), (1292, "22007"));
    assert_eq!(msg, "Incorrect datetime value: '2020-99-99'");
    assert!(!msg.contains("HINT"), "a HINT reached the wire: {msg:?}");
}
