//! v7.40.11 — the MySQL system-variable surfaces, enumerated.
//!
//! `SHOW VARIABLES` listed 19 names where MySQL 9.7.2 lists 655, and the
//! `@@name` surface was a SECOND hand-maintained table beside it. That
//! was filed as a coverage gap. Measured, it was not:
//!
//! ```text
//!   Connector/J 9.4.0 opens a connection by reading NINETEEN variables
//!   in one statement. Seven were missing. The driver never got a
//!   session — `createNewIO` threw `Unknown system variable
//!   'auto_increment_increment'`.
//! ```
//!
//! So the world's most-used MySQL client could not connect to this
//! server at all. Fixing the seven names was not enough either; the
//! measurements that followed are each pinned below, because every one
//! of them was found by running the driver rather than by reading it:
//!
//!   * `SET character_set_results = NULL` — the driver's second
//!     statement — was a syntax error.
//!   * `@@transaction_read_only`, which the driver reads before EVERY
//!     statement, did not exist.
//!   * `DatabaseMetaData.getColumns` wraps `UPPER()` around numeric CASE
//!     arms, and `upper()` refused anything but text.
//!   * the OK packet's insert id was the literal 0, so
//!     `getGeneratedKeys()` returned an empty result set for every
//!     insert.
//!
//! The first test is the one that keeps this closed: it walks whatever
//! `SHOW VARIABLES` lists and asks `@@name` for each, so a name added to
//! one surface alone fails here rather than at a customer.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Connector/J 9.4.0's connection-setup statement, captured from the
/// MySQL 9.7.2 oracle's own `general_log` while the driver connected to
/// it — not transcribed from the driver's source.
const CONNECTOR_J_SETUP: &str = "SELECT  @@session.auto_increment_increment AS auto_increment_increment, \
@@character_set_client AS character_set_client, \
@@character_set_connection AS character_set_connection, \
@@character_set_results AS character_set_results, \
@@character_set_server AS character_set_server, \
@@collation_server AS collation_server, \
@@collation_connection AS collation_connection, \
@@init_connect AS init_connect, \
@@interactive_timeout AS interactive_timeout, \
@@license AS license, \
@@lower_case_table_names AS lower_case_table_names, \
@@max_allowed_packet AS max_allowed_packet, \
@@net_write_timeout AS net_write_timeout, \
@@performance_schema AS performance_schema, \
@@sql_mode AS sql_mode, \
@@system_time_zone AS system_time_zone, \
@@time_zone AS time_zone, \
@@transaction_isolation AS transaction_isolation, \
@@wait_timeout AS wait_timeout";

fn unique_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-myvars-{label}-{nanos}"));
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

fn build_handshake_response() -> Vec<u8> {
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"anyone\0");
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

fn auth_open(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, _greet) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_handshake_response());
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "handshake refused");
    s
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &payload);
}

fn read_lenenc(buf: &[u8], pos: usize) -> (u64, usize) {
    match buf[pos] {
        0xfc => (
            u64::from(u16::from_le_bytes(
                buf[pos + 1..pos + 3].try_into().unwrap(),
            )),
            3,
        ),
        0xfd => {
            let mut bytes = [0u8; 4];
            bytes[..3].copy_from_slice(&buf[pos + 1..pos + 4]);
            (u64::from(u32::from_le_bytes(bytes)), 4)
        }
        0xfe => (
            u64::from_le_bytes(buf[pos + 1..pos + 9].try_into().unwrap()),
            9,
        ),
        n => (u64::from(n), 1),
    }
}

/// A text-protocol field. `None` is the wire's 0xfb, which is SQL NULL —
/// distinct from a zero-length string, and the difference this file
/// tests for `character_set_results`.
fn read_field(buf: &[u8], pos: usize) -> (Option<String>, usize) {
    if buf[pos] == 0xfb {
        return (None, 1);
    }
    let (n, c) = read_lenenc(buf, pos);
    let end = pos + c + n as usize;
    (
        Some(String::from_utf8_lossy(&buf[pos + c..end]).into_owned()),
        c + n as usize,
    )
}

fn is_eof(pkt: &[u8]) -> bool {
    pkt.first() == Some(&0xfe) && pkt.len() < 9
}

/// Every row of a row-returning statement. Panics with the server's own
/// message if the statement errored.
fn rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<Option<String>>> {
    send_query(s, sql);
    let (_seq, cc) = read_packet(s);
    assert_ne!(
        cc.first(),
        Some(&0xff),
        "{sql}: {}",
        String::from_utf8_lossy(&cc[1..cc.len().min(200)])
    );
    let (col_count, _) = read_lenenc(&cc, 0);
    for _ in 0..col_count {
        let _ = read_packet(s);
    }
    let (_seq, maybe_eof) = read_packet(s);
    let mut pkt = if is_eof(&maybe_eof) {
        read_packet(s).1
    } else {
        maybe_eof
    };
    let mut out = Vec::new();
    while !is_eof(&pkt) {
        let mut row = Vec::with_capacity(col_count as usize);
        let mut pos = 0;
        for _ in 0..col_count {
            let (v, c) = read_field(&pkt, pos);
            row.push(v);
            pos += c;
        }
        out.push(row);
        pkt = read_packet(s).1;
    }
    out
}

/// `(errno, sqlstate, message)` for a statement that must fail.
fn error_of(s: &mut TcpStream, sql: &str) -> (u16, String, String) {
    send_query(s, sql);
    let (_seq, pkt) = read_packet(s);
    assert_eq!(
        pkt.first(),
        Some(&0xff),
        "{sql}: expected an error packet, got {:02x?}",
        &pkt[..pkt.len().min(24)]
    );
    let errno = u16::from_le_bytes([pkt[1], pkt[2]]);
    let sqlstate = String::from_utf8_lossy(&pkt[4..9]).into_owned();
    let msg = String::from_utf8_lossy(&pkt[9..]).into_owned();
    (errno, sqlstate, msg)
}

/// `(affected_rows, last_insert_id)` from the OK packet of a statement
/// that returns no rows.
fn ok_of(s: &mut TcpStream, sql: &str) -> (u64, u64) {
    send_query(s, sql);
    let (_seq, pkt) = read_packet(s);
    assert_eq!(
        pkt.first(),
        Some(&0x00),
        "{sql}: {}",
        String::from_utf8_lossy(&pkt[1..pkt.len().min(200)])
    );
    let (affected, c1) = read_lenenc(&pkt, 1);
    let (insert_id, _) = read_lenenc(&pkt, 1 + c1);
    (affected, insert_id)
}

fn open_server(label: &str) -> (common::ChildGuard, String) {
    let dir = unique_dir(label);
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

/// The guard against the defect class: the listing and the `@@` surface
/// are ONE inventory, so every name in the listing must answer.
///
/// This is an enumeration, not a list of names — a name added to
/// `SHOW VARIABLES` alone fails here without anyone remembering to
/// extend a fixture.
#[test]
fn every_listed_variable_answers_on_the_at_at_surface() {
    let (_guard, addr) = open_server("both-surfaces");
    let mut s = auth_open(&addr);

    let listed = rows(&mut s, "SHOW VARIABLES");
    assert!(
        listed.len() > 30,
        "the inventory shrank to {} rows",
        listed.len()
    );

    for row in &listed {
        let name = row[0].as_deref().expect("Variable_name is never NULL");
        let shown = row[1].as_deref().unwrap_or("");
        // MySQL names have no dots; a dotted one is a PostgreSQL custom
        // GUC that leaked into a MySQL-shaped answer (measured: 0 of
        // MySQL 9.7.2's 655 names contain one).
        assert!(
            !name.contains('.'),
            "`{name}` is not a MySQL system-variable name"
        );
        let answered = rows(&mut s, &format!("SELECT @@{name}"));
        assert_eq!(
            answered.len(),
            1,
            "@@{name} answered {} rows",
            answered.len()
        );
        let at_at = answered[0][0].as_deref().unwrap_or("");
        // The two surfaces render a boolean differently and agree on
        // everything else — measured on MySQL 9.7.2, `SHOW VARIABLES`
        // says ON/OFF where `@@` says 1/0.
        let agrees = match shown {
            "ON" => at_at == "1",
            "OFF" => at_at == "0",
            other => at_at == other,
        };
        assert!(
            agrees,
            "one question, two answers: SHOW VARIABLES says `{name}` = {shown:?} \
             and @@{name} says {at_at:?}"
        );
    }
}

/// A MySQL client must not be shown PostgreSQL's settings.
///
/// Three names reached this listing, and every one was written by SPG
/// itself rather than by the client: `spg.database`, which every
/// mysql-wire session carries; `work_mem`, from a `-c work_mem=…` at
/// boot; and `default_transaction_read_only`, which
/// `SET transaction_read_only = 1` records. MySQL 9.7.2 has none of
/// them — measured, none of its 655 names contains a dot and its
/// `default%` and `work%` listings share nothing with these — and it
/// refuses to SET a name it does not know, so a tool that dumped this
/// listing and replayed it produced statements no MySQL would accept.
#[test]
fn the_listing_carries_no_postgresql_settings() {
    let dir = unique_dir("pg-only");
    let (child, addrs) = common::ServerBuilder::new()
        .arg("-c")
        .arg("work_mem=64MB")
        .arg_path(&dir.join("d.spgdb"))
        .with_mysqlwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    let mut s = auth_open(&addr);

    // The session has all three set by the time this runs.
    ok_of(&mut s, "SET SESSION transaction_read_only = 1");

    let listed = rows(&mut s, "SHOW VARIABLES");
    let names: Vec<String> = listed
        .iter()
        .map(|r| r[0].clone().expect("Variable_name is never NULL"))
        .collect();
    for absent in ["spg.database", "work_mem", "default_transaction_read_only"] {
        assert!(
            !names.iter().any(|n| n == absent),
            "`{absent}` is PostgreSQL's, and MySQL 9.7.2 has no such variable: {names:?}"
        );
    }
    // The control: the MySQL name for the same state IS there, and it
    // reports what was set — so the filter above removed a spelling,
    // not the answer.
    assert!(names.iter().any(|n| n == "transaction_read_only"));
    assert_eq!(
        rows(&mut s, "SELECT @@transaction_read_only")[0][0].as_deref(),
        Some("1")
    );
}

/// The statement that could not run: Connector/J's connection setup.
#[test]
fn the_connector_j_setup_statement_runs() {
    let (_guard, addr) = open_server("connectorj");
    let mut s = auth_open(&addr);

    let answered = rows(&mut s, CONNECTOR_J_SETUP);
    assert_eq!(answered.len(), 1, "the driver expects exactly one row");
    assert_eq!(
        answered[0].len(),
        19,
        "the driver reads nineteen variables and every one has to answer"
    );
    // A driver that cannot tell a value from an absence is a driver that
    // guesses: none of the nineteen may come back NULL.
    for (i, v) in answered[0].iter().enumerate() {
        assert!(v.is_some(), "field {i} of the driver's setup row was NULL");
    }
}

/// `SET character_set_results = NULL` — the driver's second statement,
/// and the one spelling of it MySQL accepts.
#[test]
fn character_set_results_is_the_one_variable_that_takes_null() {
    let (_guard, addr) = open_server("csr-null");
    let mut s = auth_open(&addr);

    let (affected, _) = ok_of(&mut s, "SET character_set_results = NULL");
    assert_eq!(affected, 0);

    // Measured on MySQL 9.7.2: `@@character_set_results IS NULL` is 1
    // while `SHOW VARIABLES` renders an empty value for the same state.
    let answered = rows(&mut s, "SELECT @@character_set_results");
    assert_eq!(
        answered[0][0], None,
        "the `@@` surface must answer SQL NULL, not an empty string"
    );
    let listed = rows(&mut s, "SHOW VARIABLES LIKE 'character_set_results'");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0][1].as_deref(), Some(""));

    // And every other name refuses it, with MySQL's own error.
    let (errno, sqlstate, msg) = error_of(&mut s, "SET sql_mode = NULL");
    assert_eq!(errno, 1231, "MySQL 9.7.2 answers 1231 here (measured)");
    assert_eq!(sqlstate, "42000");
    assert!(
        msg.contains("Variable 'sql_mode' can't be set to the value of 'NULL'"),
        "unexpected message: {msg}"
    );
}

/// The OK packet's insert id, which JDBC hands back as
/// `getGeneratedKeys()`. Every value below was measured against MySQL
/// 9.7.2 through a driver reading the same field.
#[test]
fn the_ok_packet_carries_the_key_this_statement_made() {
    let (_guard, addr) = open_server("insert-id");
    let mut s = auth_open(&addr);

    ok_of(
        &mut s,
        "CREATE TABLE k (id INT AUTO_INCREMENT PRIMARY KEY, n INT)",
    );
    assert_eq!(ok_of(&mut s, "INSERT INTO k (n) VALUES (1)"), (1, 1));
    assert_eq!(ok_of(&mut s, "INSERT INTO k (n) VALUES (2)"), (1, 2));
    // An EXPLICIT key is reported here even though it leaves
    // `LAST_INSERT_ID()` alone.
    assert_eq!(
        ok_of(&mut s, "INSERT INTO k (id, n) VALUES (100, 3)"),
        (1, 100)
    );
    // A statement that made no key reports none — the stale value would
    // be a wrong generated key, which is worse than no key.
    assert_eq!(ok_of(&mut s, "UPDATE k SET n = n + 1"), (3, 0));
    assert_eq!(ok_of(&mut s, "DELETE FROM k WHERE id = 100"), (1, 0));
    assert_eq!(
        ok_of(&mut s, "INSERT INTO k (n) VALUES (7),(8),(9)"),
        (3, 101),
        "a multi-row insert reports the FIRST key it made"
    );
    // The other quantity, unchanged by any of the above: it still holds
    // the last GENERATED value.
    assert_eq!(
        rows(&mut s, "SELECT LAST_INSERT_ID()")[0][0].as_deref(),
        Some("101")
    );
}

/// `SET SESSION transaction_read_only = 1` reached nothing. The engine
/// has refused a write in a read-only transaction since v7.39 —
/// measured over pgwire, both `BEGIN READ ONLY` and
/// `SET default_transaction_read_only = on` do — and the MySQL spelling
/// was stored, echoed back, and ignored. A connection pool doing
/// read/write splitting marks a connection this way before routing it
/// to a replica.
///
/// Every reading below is MySQL 9.7.2's, measured through a driver.
#[test]
fn a_read_only_mysql_session_refuses_a_write() {
    let (_guard, addr) = open_server("read-only");
    let mut s = auth_open(&addr);

    ok_of(&mut s, "CREATE TABLE ro (n INT)");
    assert_eq!(
        rows(&mut s, "SELECT @@transaction_read_only")[0][0].as_deref(),
        Some("0"),
        "a fresh session is read-write"
    );

    ok_of(&mut s, "SET SESSION transaction_read_only = 1");
    assert_eq!(
        rows(&mut s, "SELECT @@transaction_read_only")[0][0].as_deref(),
        Some("1")
    );
    ok_of(&mut s, "START TRANSACTION");
    let (errno, sqlstate, msg) = error_of(&mut s, "INSERT INTO ro VALUES (1)");
    assert_eq!(errno, 1792);
    assert_eq!(sqlstate, "25006");
    assert!(
        msg.contains("Cannot execute statement in a READ ONLY transaction"),
        "unexpected message: {msg}"
    );
    ok_of(&mut s, "ROLLBACK");

    // The control: turn it off and the same write lands, so the test
    // above is reading a refusal rather than a broken session.
    ok_of(&mut s, "SET SESSION transaction_read_only = 0");
    assert_eq!(ok_of(&mut s, "INSERT INTO ro VALUES (2)").0, 1);
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM ro")[0][0].as_deref(),
        Some("1"),
        "exactly the row that was allowed"
    );
}

/// `UPPER()` over a non-text value, which is what
/// `DatabaseMetaData.getColumns` does to its numeric CASE arms.
#[test]
fn upper_takes_any_type_in_the_mysql_dialect() {
    let (_guard, addr) = open_server("upper");
    let mut s = auth_open(&addr);

    // Measured on MySQL 9.7.2: `123`, `45.6`, `1`, NULL.
    let answered = rows(
        &mut s,
        "SELECT UPPER(123), LOWER(45.6), UPPER(TRUE), UPPER(NULL)",
    );
    assert_eq!(answered[0][0].as_deref(), Some("123"));
    assert_eq!(answered[0][1].as_deref(), Some("45.6"));
    assert_eq!(answered[0][2].as_deref(), Some("1"));
    assert_eq!(answered[0][3], None);
}
