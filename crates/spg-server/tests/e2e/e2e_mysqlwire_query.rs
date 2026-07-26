//! v7.17.0 Phase 3.P0-73 — COM_QUERY text-protocol round-trip.
//!
//! After a clean handshake the server enters the command phase
//! and accepts COM_QUERY (0x03). The server replies with either:
//!   * an ERR packet for parse / engine errors,
//!   * an OK packet (CommandOk) for DDL / DML,
//!   * a text result set (column_count + column_def_41 * N +
//!     row_packet * M + trailing OK) for SELECT.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-mysqlwire-query-{label}-{pid}-{nanos}"));
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
    payload.push(0); // auth_response empty → open mode accepts
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

/// Walk a handshake, leaving the stream in the command phase.
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

/// Read a length-encoded integer starting at `pos`; returns the
/// value and how many bytes it consumed.
fn read_lenenc(buf: &[u8], pos: usize) -> (u64, usize) {
    let first = buf[pos];
    match first {
        0xfb => (0, 1), // NULL — handled by caller
        0xfc => {
            let v = u16::from_le_bytes(buf[pos + 1..pos + 3].try_into().unwrap());
            (u64::from(v), 3)
        }
        0xfd => {
            let mut bytes = [0u8; 4];
            bytes[..3].copy_from_slice(&buf[pos + 1..pos + 4]);
            let v = u32::from_le_bytes(bytes);
            (u64::from(v), 4)
        }
        0xfe => {
            let v = u64::from_le_bytes(buf[pos + 1..pos + 9].try_into().unwrap());
            (v, 9)
        }
        n => (u64::from(n), 1),
    }
}

fn read_lenenc_string(buf: &[u8], pos: usize) -> (Vec<u8>, usize) {
    let (n, consumed) = read_lenenc(buf, pos);
    let s = buf[pos + consumed..pos + consumed + n as usize].to_vec();
    (s, consumed + n as usize)
}

/// v7.39 (round 504) — consume the marker that closes a result set's column
/// definitions.
///
/// This harness does not take CLIENT_DEPRECATE_EOF (see
/// `build_handshake_response`), and neither does MariaDB's own client
/// library, so the server must send one. Measured off MariaDB 11 answering
/// `SELECT 1` on such a connection: `fe 00 00 02 00`.
///
/// Before round 504 SPG left it out for every client, because it framed
/// result sets against the capabilities it ADVERTISED rather than the ones
/// the client took — and these tests asserted that, so the suite was green
/// while a real `mariadb` CLI could not read `SELECT 1` at all.
fn read_columns_eof(s: &mut TcpStream) {
    let (_seq, pkt) = read_packet(s);
    assert_eq!(pkt[0], 0xfe, "EOF closes the column definitions");
    assert_eq!(pkt.len(), 5, "protocol-41 EOF: header + warnings + status");
}

/// The trailing marker of a result set, for a client without
/// CLIENT_DEPRECATE_EOF: an EOF packet, not an OK packet.
fn read_result_eof(s: &mut TcpStream) {
    read_columns_eof(s);
}

/// True for the EOF that ends the rows. A text row could only begin with
/// 0xfe if its first column were 16 MB or larger, which is why the length
/// is part of the test — that is how real clients tell them apart.
fn is_result_eof(pkt: &[u8]) -> bool {
    pkt[0] == 0xfe && pkt.len() < 9
}

#[test]
fn select_literal_int_returns_one_column_one_row() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "SELECT 42 AS answer");
    let (_seq, cc_pkt) = read_packet(&mut s);
    let (col_count, _) = read_lenenc(&cc_pkt, 0);
    assert_eq!(col_count, 1, "1 projection column");
    let (_seq, col_def) = read_packet(&mut s);
    // Parse column_def_41: 5 lenenc strings + 0x0c marker + type bytes.
    let mut pos = 0;
    let (_catalog, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    let (_schema, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    let (_table, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    let (_org_table, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    let (name, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    assert_eq!(name, b"answer");
    let (_org_name, c) = read_lenenc_string(&col_def, pos);
    pos += c;
    assert_eq!(col_def[pos], 0x0c, "fixed-length marker");
    // Skip charset (2), length (4), type byte (1)
    let type_byte = col_def[pos + 1 + 2 + 4];
    // SPG widens integer literals fitting in i32 to MYSQL_TYPE_LONG (0x03).
    assert_eq!(type_byte, 0x03, "MYSQL_TYPE_LONG");
    read_columns_eof(&mut s);
    let (_seq, row) = read_packet(&mut s);
    let (value, _) = read_lenenc_string(&row, 0);
    assert_eq!(value, b"42");
    let (_seq, eof) = read_packet(&mut s);
    assert_eq!(eof[0], 0xfe, "trailing EOF");
}

#[test]
fn select_text_literal_round_trips_through_lenenc_string() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "SELECT 'hello, mysql' AS greeting");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    read_columns_eof(&mut s);
    let (_seq, row) = read_packet(&mut s);
    let (value, _) = read_lenenc_string(&row, 0);
    assert_eq!(value, b"hello, mysql");
    read_result_eof(&mut s);
}

#[test]
fn ddl_returns_ok_packet_with_affected_zero() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "OK header");
    // affected_rows + last_insert_id are both single-byte 0.
    assert_eq!(ok[1], 0, "affected = 0");
}

#[test]
fn dml_returns_ok_packet_with_affected_count() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let (_seq, _ok) = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (1), (2), (3)");
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
    let (affected, _) = read_lenenc(&ok, 1);
    assert_eq!(affected, 3);
}

#[test]
fn select_from_table_returns_correct_rows() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "CREATE TABLE products (id INT NOT NULL, name TEXT)");
    let (_seq, _ok) = read_packet(&mut s);
    send_query(
        &mut s,
        "INSERT INTO products VALUES (10, 'widget'), (20, 'gadget'), (30, 'doohickey')",
    );
    let (_seq, _ok) = read_packet(&mut s);
    send_query(&mut s, "SELECT id, name FROM products ORDER BY id");
    let (_seq, cc) = read_packet(&mut s);
    let (col_count, _) = read_lenenc(&cc, 0);
    assert_eq!(col_count, 2);
    let (_seq, _col1) = read_packet(&mut s);
    let (_seq, _col2) = read_packet(&mut s);
    read_columns_eof(&mut s);
    let mut got_rows: Vec<(String, String)> = Vec::new();
    loop {
        let (_seq, pkt) = read_packet(&mut s);
        if is_result_eof(&pkt) {
            break;
        }
        let (id_bytes, c) = read_lenenc_string(&pkt, 0);
        let (name_bytes, _) = read_lenenc_string(&pkt, c);
        got_rows.push((
            String::from_utf8(id_bytes).unwrap(),
            String::from_utf8(name_bytes).unwrap(),
        ));
    }
    assert_eq!(
        got_rows,
        vec![
            ("10".to_string(), "widget".to_string()),
            ("20".to_string(), "gadget".to_string()),
            ("30".to_string(), "doohickey".to_string()),
        ]
    );
}

#[test]
fn null_values_decode_as_0xfb_byte() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "CREATE TABLE t (a INT, b TEXT)");
    let (_seq, _ok) = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (NULL, 'x'), (1, NULL)");
    let (_seq, _ok) = read_packet(&mut s);
    send_query(&mut s, "SELECT a, b FROM t ORDER BY b");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _c1) = read_packet(&mut s);
    let (_seq, _c2) = read_packet(&mut s);
    read_columns_eof(&mut s);
    let (_seq, row1) = read_packet(&mut s);
    // v7.39 (round 403) — the MySQL wire session sorts NULL FIRST for ASC
    // (NULL is the smallest value), so the `b IS NULL` row `(1, NULL)` comes
    // before `(NULL, 'x')`. row1 is '1' + NULL(0xfb).
    let (a1, c) = read_lenenc_string(&row1, 0);
    assert_eq!(a1, b"1");
    assert_eq!(row1[c], 0xfb, "NULL text column");
    let (_seq, row2) = read_packet(&mut s);
    // row2 is NULL(0xfb) int + 'x'.
    assert_eq!(row2[0], 0xfb, "NULL int column");
    let (b2, _) = read_lenenc_string(&row2, 1);
    assert_eq!(b2, b"x");
    read_result_eof(&mut s);
}

#[test]
fn parse_error_returns_err_packet() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "SELEKT 1");
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1064, "ER_PARSE_ERROR");
    assert_eq!(&err[4..9], b"42000");
}

#[test]
fn com_quit_closes_connection_cleanly() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // COM_QUIT (0x01) — server closes the connection without
    // replying.
    write_packet(&mut s, 0, &[0x01]);
    // Reading should return EOF (0 bytes).
    let mut buf = [0u8; 4];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "server closed connection after COM_QUIT");
}

/// Read a single-column, single-row string result off the wire and
/// return the decoded value. Consumes the trailing OK.
fn query_scalar(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let (_seq, cc) = read_packet(s);
    let (col_count, _) = read_lenenc(&cc, 0);
    for _ in 0..col_count {
        let _ = read_packet(s);
    }
    read_columns_eof(s);
    let (_seq, row) = read_packet(s);
    let (val, _) = read_lenenc_string(&row, 0);
    read_result_eof(s);
    String::from_utf8(val).unwrap()
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let (_seq, ok) = read_packet(s);
    assert_eq!(ok[0], 0x00, "expected OK for `{sql}`, got {:#x}", ok[0]);
}

/// V15 (round 302) — a MySQL-protocol connection defaults to MySQL
/// string semantics: backslash is an escape character. Before this the
/// mysql-wire path ran on the shared PG session where `'\n'` was two
/// literal bytes. Expected values verified against MariaDB 11.
#[test]
fn mysql_connection_defaults_to_backslash_escape_dialect() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // `\n` is a single newline byte, not backslash + n.
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\n')"), "1");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\t')"), "1");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\\')"), "1");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\'')"), "1");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('a\nb')"), "3");
}

/// V15 — `SET sql_mode='NO_BACKSLASH_ESCAPES'` turns the escapes back
/// off within the session; any other sql_mode (or an empty list) leaves
/// them on. Verified vs MariaDB 11.
#[test]
fn mysql_no_backslash_escapes_sql_mode_disables_escapes() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\n')"), "1");
    exec_ok(&mut s, "SET sql_mode='NO_BACKSLASH_ESCAPES'");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\n')"), "2");
    // A different sql_mode replaces the whole value → escapes back on.
    exec_ok(&mut s, "SET sql_mode='STRICT_TRANS_TABLES'");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\n')"), "1");
    // NO_BACKSLASH_ESCAPES anywhere in the list still disables them.
    exec_ok(&mut s, "SET sql_mode='ANSI_QUOTES,NO_BACKSLASH_ESCAPES'");
    assert_eq!(query_scalar(&mut s, r"SELECT LENGTH('\n')"), "2");
}

/// V15 — the dialect is per-session, not a process-global flag. The
/// server holds one shared Engine (see the r279/r283 session-bag work),
/// so a second mysql connection must not inherit the first's
/// `NO_BACKSLASH_ESCAPES`. Guards against the "shared engine leaks
/// per-connection state" class of bug.
#[test]
fn mysql_dialect_is_isolated_per_connection() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    exec_ok(&mut a, "SET sql_mode='NO_BACKSLASH_ESCAPES'");
    assert_eq!(query_scalar(&mut a, r"SELECT LENGTH('\n')"), "2");
    // A fresh connection starts from the MySQL default (escapes on),
    // uncontaminated by A's session.
    let mut b = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut b, r"SELECT LENGTH('\n')"), "1");
    // A's session is unchanged by B's traffic.
    assert_eq!(query_scalar(&mut a, r"SELECT LENGTH('\n')"), "2");
}

/// V22 (round 303) — each mysql connection has its own transaction
/// slot. Before this every mysql statement ran on the shared slot 0, so
/// a second connection's `BEGIN` collided with `a transaction is already
/// open`. pgwire has had per-connection slots since r283.
#[test]
fn two_mysql_connections_can_each_hold_a_transaction() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    let mut b = auth_open_mode(&addr);
    exec_ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    // Both connections open a transaction concurrently — no collision.
    exec_ok(&mut a, "BEGIN");
    exec_ok(&mut b, "BEGIN");
    exec_ok(&mut a, "INSERT INTO t VALUES (1, 100)");
    exec_ok(&mut b, "INSERT INTO t VALUES (2, 200)");
    // Each sees only its own uncommitted row (READ COMMITTED isolation,
    // same engine machinery pgwire uses).
    assert_eq!(query_scalar(&mut a, "SELECT COUNT(*) FROM t"), "1");
    assert_eq!(query_scalar(&mut b, "SELECT COUNT(*) FROM t"), "1");
    exec_ok(&mut a, "COMMIT");
    exec_ok(&mut b, "COMMIT");
    // After both commit, both rows are visible.
    assert_eq!(query_scalar(&mut a, "SELECT COUNT(*) FROM t"), "2");
}

/// V22 — BEGIN/ROLLBACK discards; the connection's slot is independent
/// so a rollback on one connection doesn't touch another's committed
/// data. Verified vs MariaDB 11 (BEGIN;INSERT;ROLLBACK → row gone).
#[test]
fn mysql_transaction_rollback_discards_only_its_own_writes() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    let mut b = auth_open_mode(&addr);
    exec_ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY)");
    exec_ok(&mut a, "INSERT INTO t VALUES (1)");
    exec_ok(&mut b, "BEGIN");
    exec_ok(&mut b, "INSERT INTO t VALUES (2)");
    exec_ok(&mut b, "ROLLBACK");
    // b's uncommitted row is gone; a's committed row survives.
    assert_eq!(query_scalar(&mut a, "SELECT COUNT(*) FROM t"), "1");
}

/// V22 — a transaction left open at disconnect is rolled back, matching
/// pgwire's backend-exit cleanup. The row must not leak to the next
/// connection.
#[test]
fn mysql_open_transaction_rolls_back_on_disconnect() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    exec_ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY)");
    exec_ok(&mut a, "BEGIN");
    exec_ok(&mut a, "INSERT INTO t VALUES (1)");
    // Disconnect abruptly with the transaction still open.
    write_packet(&mut a, 0, &[0x01]); // COM_QUIT
    drop(a);
    // A fresh connection sees no uncommitted row.
    let mut b = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut b, "SELECT COUNT(*) FROM t"), "0");
}

#[test]
fn unknown_command_returns_err_packet() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // Send unknown command 0x99.
    write_packet(&mut s, 0, &[0x99, b'X']);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1047);
}

/// Read the `status_flags` out of an OK packet. Layout after the 0x00
/// header: affected_rows (lenenc), last_insert_id (lenenc), then the
/// two-byte flags.
fn ok_status(pkt: &[u8]) -> u16 {
    assert_eq!(pkt[0], 0x00, "expected an OK packet, got {:#x}", pkt[0]);
    let mut pos = 1;
    for _ in 0..2 {
        let (_, used) = read_lenenc(pkt, pos);
        pos += used;
    }
    u16::from_le_bytes([pkt[pos], pkt[pos + 1]])
}

/// Run a statement that answers a bare OK and return its status flags.
fn status_of(s: &mut TcpStream, sql: &str) -> u16 {
    send_query(s, sql);
    let (_seq, pkt) = read_packet(s);
    ok_status(&pkt)
}

/// The status flags on an EOF packet: 0xfe + 2-byte warnings + 2-byte status.
fn eof_status(pkt: &[u8]) -> u16 {
    assert_eq!(pkt[0], 0xfe, "expected an EOF packet, got {:#x}", pkt[0]);
    u16::from_le_bytes([pkt[3], pkt[4]])
}

/// Run a SELECT and return the status flags on its TERMINATING packet.
fn status_of_select(s: &mut TcpStream, sql: &str) -> u16 {
    send_query(s, sql);
    let (_seq, cc) = read_packet(s);
    let (col_count, _) = read_lenenc(&cc, 0);
    for _ in 0..col_count {
        let _ = read_packet(s);
    }
    read_columns_eof(s);
    loop {
        let (_seq, pkt) = read_packet(s);
        if is_result_eof(&pkt) {
            return eof_status(&pkt);
        }
    }
}

const IN_TRANS: u16 = 0x0001;
const AUTOCOMMIT: u16 = 0x0002;

/// V37 (round 316) — the OK packet's status flags carry the transaction
/// bit. They used to be a constant `AUTOCOMMIT`, so a client that probes
/// the server's view of the connection — which is what several pools do
/// to decide whether a pooled connection is clean — was told "no
/// transaction" throughout a block.
///
/// Measured against MariaDB 11: the bit is set on the BEGIN's own reply,
/// on every OK inside the block, and cleared by COMMIT and ROLLBACK.
#[test]
fn the_ok_packet_reports_the_transaction_state() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE TABLE t316 (id INT)");
    // NB a SELECT answers a result SET, so its flags ride the
    // terminator; only OK-shaped statements go through `status_of`.
    assert_eq!(status_of_select(&mut s, "SELECT 1"), AUTOCOMMIT, "idle");

    // The BEGIN's own reply already says IN_TRANS.
    assert_eq!(
        status_of(&mut s, "BEGIN"),
        AUTOCOMMIT | IN_TRANS,
        "BEGIN's own reply"
    );
    assert_eq!(
        status_of(&mut s, "INSERT INTO t316 VALUES (1)"),
        AUTOCOMMIT | IN_TRANS,
        "inside the block"
    );
    // …and COMMIT's reply says it is over.
    assert_eq!(status_of(&mut s, "COMMIT"), AUTOCOMMIT, "COMMIT clears it");

    // ROLLBACK closes it the same way.
    assert_eq!(status_of(&mut s, "BEGIN"), AUTOCOMMIT | IN_TRANS);
    assert_eq!(status_of(&mut s, "ROLLBACK"), AUTOCOMMIT, "ROLLBACK clears it");
    assert_eq!(status_of_select(&mut s, "SELECT 1"), AUTOCOMMIT, "idle again");
}

/// A result set's terminating packet carries the flags too — measured
/// against MariaDB, where a SELECT inside a block answers IN_TRANS on
/// its terminator.
#[test]
fn a_result_sets_terminator_reports_it_too() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(status_of_select(&mut s, "SELECT 1"), AUTOCOMMIT);
    exec_ok(&mut s, "BEGIN");
    assert_eq!(
        status_of_select(&mut s, "SELECT 1"),
        AUTOCOMMIT | IN_TRANS,
        "SELECT inside a block"
    );
    exec_ok(&mut s, "COMMIT");
    assert_eq!(status_of_select(&mut s, "SELECT 1"), AUTOCOMMIT);
}

/// COM_PING reports it as well — MariaDB's does, and a pool that pings
/// to check liveness reads the same field.
#[test]
fn com_ping_reports_the_transaction_state() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    write_packet(&mut s, 0, &[0x0e]);
    let (_seq, pkt) = read_packet(&mut s);
    assert_eq!(ok_status(&pkt), AUTOCOMMIT, "ping when idle");

    exec_ok(&mut s, "BEGIN");
    write_packet(&mut s, 0, &[0x0e]);
    let (_seq, pkt) = read_packet(&mut s);
    assert_eq!(ok_status(&pkt), AUTOCOMMIT | IN_TRANS, "ping inside a block");
    exec_ok(&mut s, "ROLLBACK");
}

/// The bit is per-CONNECTION. A second client's open block must not make
/// this one look dirty — the same global-vs-slot trap rounds 298 and 304
/// each had to fix elsewhere.
#[test]
fn one_connections_block_does_not_colour_anothers_status() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    let mut b = auth_open_mode(&addr);
    exec_ok(&mut a, "BEGIN");
    assert_eq!(status_of_select(&mut a, "SELECT 1"), AUTOCOMMIT | IN_TRANS);
    assert_eq!(
        status_of_select(&mut b, "SELECT 1"),
        AUTOCOMMIT,
        "B is idle and must say so while A holds a block"
    );
    exec_ok(&mut a, "ROLLBACK");
}

/// Walk a handshake like [`auth_open_mode`] but also return the
/// `connection_id` the greeting announced.
fn auth_open_mode_with_id(addr: &str) -> (TcpStream, u32) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, greeting) = read_packet(&mut s);
    // HandshakeV10: protocol_version(1) + server_version NUL-string +
    // connection_id (4, LE).
    let nul = 1 + greeting[1..].iter().position(|&b| b == 0).expect("version NUL");
    let idpos = nul + 1;
    let conn_id = u32::from_le_bytes(greeting[idpos..idpos + 4].try_into().unwrap());
    write_packet(&mut s, 1, &build_handshake_response("anyone"));
    let (_seqno, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "expected OK after auth, got {:#x}", ok[0]);
    (s, conn_id)
}

/// Read a whole text result set. This client takes no CLIENT_DEPRECATE_EOF,
/// so an EOF closes the column definitions and another closes the rows.
fn query_rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<Option<String>>> {
    send_query(s, sql);
    let (_seq, cc) = read_packet(s);
    let (col_count, _) = read_lenenc(&cc, 0);
    for _ in 0..col_count {
        let _ = read_packet(s);
    }
    read_columns_eof(s);
    let mut out = Vec::new();
    loop {
        let (_seq, pkt) = read_packet(s);
        if is_result_eof(&pkt) {
            return out;
        }
        let mut pos = 0;
        let mut row = Vec::with_capacity(col_count as usize);
        for _ in 0..col_count {
            if pkt[pos] == 0xfb {
                row.push(None);
                pos += 1;
            } else {
                let (v, used) = read_lenenc_string(&pkt, pos);
                pos += used;
                row.push(Some(String::from_utf8(v).unwrap()));
            }
        }
        out.push(row);
    }
}

/// V36 (round 317) — the connection id is real. It used to be the
/// LISTENER's port, so every connection was handed the same id in the
/// greeting and `CONNECTION_ID()` answered a hardcoded 1 regardless:
/// nothing could name one specific connection.
///
/// Measured against MariaDB 11: distinct connections get distinct ids,
/// the id is stable inside a connection, and it is the same number the
/// greeting announced.
#[test]
fn connection_id_is_per_connection_and_matches_the_greeting() {
    let (_guard, addr) = spawn();
    let (mut a, a_id) = auth_open_mode_with_id(&addr);
    let (mut b, b_id) = auth_open_mode_with_id(&addr);

    assert_ne!(a_id, b_id, "two live connections must not share an id");

    let a_seen = query_scalar(&mut a, "SELECT connection_id()");
    let b_seen = query_scalar(&mut b, "SELECT connection_id()");
    assert_eq!(
        a_seen,
        a_id.to_string(),
        "CONNECTION_ID() must be the id the greeting announced"
    );
    assert_eq!(b_seen, b_id.to_string());
    assert_eq!(
        query_scalar(&mut a, "SELECT connection_id()"),
        a_seen,
        "stable within the connection"
    );
}

/// V36 (round 317) — `SHOW PROCESSLIST` reports the LIVE connections.
/// It used to be one hardcoded row (Id 1, user "postgres", Info
/// "SHOW PROCESSLIST") whatever was attached, so the surface an operator
/// reaches for to find a connection could never show one.
///
/// Measured against MariaDB 11: the asking connection's own row carries
/// its `CONNECTION_ID()` and the statement text in `Info`; an idle
/// connection reports Command `Sleep` with NULL `Info`.
#[test]
fn show_processlist_lists_the_live_connections() {
    let (_guard, addr) = spawn();
    let (mut a, a_id) = auth_open_mode_with_id(&addr);
    let (mut b, b_id) = auth_open_mode_with_id(&addr);
    // Make B do something first so it is fully through its handshake.
    assert_eq!(query_scalar(&mut b, "SELECT 1"), "1");

    let rows = query_rows(&mut a, "SHOW PROCESSLIST");
    let find = |id: u32| {
        rows.iter()
            .find(|r| r[0].as_deref() == Some(id.to_string().as_str()))
            .unwrap_or_else(|| panic!("no row for connection {id} in {rows:?}"))
            .clone()
    };
    let a_row = find(a_id);
    let b_row = find(b_id);

    assert_eq!(a_row[4].as_deref(), Some("Query"), "the asker is running one");
    assert_eq!(
        a_row[7].as_deref(),
        Some("SHOW PROCESSLIST"),
        "its own Info is the statement it is running"
    );
    assert_eq!(b_row[4].as_deref(), Some("Sleep"), "B is between statements");
    assert_eq!(b_row[7], None, "an idle connection has no Info");
}

/// A connection that goes away leaves the list — otherwise the registry
/// would grow forever and report dead connections as live.
#[test]
fn a_closed_connection_drops_out_of_the_processlist() {
    let (_guard, addr) = spawn();
    let (mut a, _a_id) = auth_open_mode_with_id(&addr);
    let (mut b, b_id) = auth_open_mode_with_id(&addr);
    assert_eq!(query_scalar(&mut b, "SELECT 1"), "1");
    assert!(
        query_rows(&mut a, "SHOW PROCESSLIST")
            .iter()
            .any(|r| r[0].as_deref() == Some(b_id.to_string().as_str())),
        "B is live"
    );

    // COM_QUIT, then wait for the server to notice the socket closed.
    write_packet(&mut b, 0, &[0x01]);
    drop(b);
    let mut gone = false;
    for _ in 0..100 {
        gone = !query_rows(&mut a, "SHOW PROCESSLIST")
            .iter()
            .any(|r| r[0].as_deref() == Some(b_id.to_string().as_str()));
        if gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gone, "a disconnected connection must leave the processlist");
}

/// Read an ERR packet: `0xff`, errno (2, LE), `#`, 5-byte SQLSTATE, message.
fn err_parts(pkt: &[u8]) -> (u16, String, String) {
    assert_eq!(pkt[0], 0xff, "expected an ERR packet, got {:#x}", pkt[0]);
    let errno = u16::from_le_bytes([pkt[1], pkt[2]]);
    assert_eq!(pkt[3], b'#', "SQLSTATE marker");
    let sqlstate = String::from_utf8(pkt[4..9].to_vec()).unwrap();
    let msg = String::from_utf8(pkt[9..].to_vec()).unwrap();
    (errno, sqlstate, msg)
}

fn err_of(s: &mut TcpStream, sql: &str) -> (u16, String, String) {
    send_query(s, sql);
    let (_seq, pkt) = read_packet(s);
    err_parts(&pkt)
}

/// V51 (round 318) — `KILL` exists. The statement used to fail to parse at
/// all, so the documented MariaDB way to drop a runaway connection was a
/// syntax error against SPG.
///
/// Measured against MariaDB 11: an id no connection carries is
/// `ERROR 1094 (HY000) Unknown thread id: N`, for both the CONNECTION and
/// the QUERY form.
#[test]
fn kill_of_an_unknown_thread_id_is_error_1094() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    for sql in ["KILL 999999", "KILL QUERY 999999", "KILL CONNECTION 999999"] {
        let (errno, sqlstate, msg) = err_of(&mut s, sql);
        assert_eq!((errno, sqlstate.as_str()), (1094, "HY000"), "for `{sql}`");
        assert_eq!(msg, "Unknown thread id: 999999", "for `{sql}`");
    }
    // Still usable afterwards.
    assert_eq!(query_scalar(&mut s, "SELECT 1"), "1");
}

/// `KILL CONNECTION <other>` drops that connection and leaves the killer
/// alone. Measured against MariaDB 11: the victim gets no ERR packet, its
/// connection is simply closed.
#[test]
fn kill_connection_drops_the_named_connection() {
    let (_guard, addr) = spawn();
    let (mut killer, _) = auth_open_mode_with_id(&addr);
    let (mut victim, victim_id) = auth_open_mode_with_id(&addr);
    assert_eq!(query_scalar(&mut victim, "SELECT 1"), "1");

    exec_ok(&mut killer, &format!("KILL CONNECTION {victim_id}"));

    // The victim is gone: it leaves the processlist, and naming it again
    // is an unknown thread id.
    let mut gone = false;
    for _ in 0..100 {
        gone = !query_rows(&mut killer, "SHOW PROCESSLIST")
            .iter()
            .any(|r| r[0].as_deref() == Some(victim_id.to_string().as_str()));
        if gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gone, "a killed connection must leave the processlist");
    let (errno, _, _) = err_of(&mut killer, &format!("KILL CONNECTION {victim_id}"));
    assert_eq!(errno, 1094, "the id is no longer live");

    // The victim's socket really is closed — a further exchange fails.
    send_query(&mut victim, "SELECT 1");
    let mut hdr = [0u8; 4];
    assert!(
        victim.read_exact(&mut hdr).is_err(),
        "the killed connection must be closed"
    );

    // And the killer is untouched.
    assert_eq!(query_scalar(&mut killer, "SELECT 1"), "1");
}

/// Killing your own connection: MariaDB 11 answers
/// `ERROR 1927 (70100) Connection was killed` and closes.
#[test]
fn kill_of_your_own_connection_reports_1927_and_closes() {
    let (_guard, addr) = spawn();
    let (mut s, my_id) = auth_open_mode_with_id(&addr);
    let (errno, sqlstate, msg) = err_of(&mut s, &format!("KILL CONNECTION {my_id}"));
    assert_eq!((errno, sqlstate.as_str()), (1927, "70100"));
    assert_eq!(msg, "Connection was killed");

    send_query(&mut s, "SELECT 1");
    let mut hdr = [0u8; 4];
    assert!(
        s.read_exact(&mut hdr).is_err(),
        "the connection must be closed after killing itself"
    );
}

/// `KILL QUERY <self>` only stops a running statement — the connection
/// survives, so the next statement works.
#[test]
fn kill_query_leaves_the_connection_alive() {
    let (_guard, addr) = spawn();
    let (mut s, my_id) = auth_open_mode_with_id(&addr);
    exec_ok(&mut s, &format!("KILL QUERY {my_id}"));
    assert_eq!(
        query_scalar(&mut s, "SELECT 1"),
        "1",
        "KILL QUERY must not end the connection"
    );
}

/// V52 (round 319) — the Host and db columns describe the connection.
/// `Host` was a hardcoded "localhost" and `db` a hardcoded "postgres" for
/// every row. Measured on MariaDB 11: `Host` is `addr:port` for a TCP
/// client, `db` is the database that connection selected, NULL when it
/// selected none.
#[test]
fn processlist_host_and_db_describe_the_connection() {
    let (_guard, addr) = spawn();
    let (mut s, my_id) = auth_open_mode_with_id(&addr);
    let local = s.local_addr().unwrap();

    let own_row = |s: &mut TcpStream| {
        query_rows(s, "SHOW PROCESSLIST")
            .into_iter()
            .find(|r| r[0].as_deref() == Some(my_id.to_string().as_str()))
            .expect("our own row")
    };

    let row = own_row(&mut s);
    assert_eq!(
        row[2].as_deref(),
        Some(format!("{}:{}", local.ip(), local.port()).as_str()),
        "Host is the peer address"
    );
    assert_eq!(row[3], None, "no database selected yet");

    // COM_INIT_DB — what a client sends for `USE shop`.
    let mut pkt = vec![0x02];
    pkt.extend_from_slice(b"shop");
    write_packet(&mut s, 0, &pkt);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "COM_INIT_DB accepted");

    assert_eq!(
        own_row(&mut s)[3].as_deref(),
        Some("shop"),
        "db follows the selected database"
    );
}

/// V24 (round 323) — a MySQL client is not handed SPG's internal error
/// vocabulary. It used to receive the whole layering, e.g.
/// `parse: parse error at token #3: expected identifier, got Eof`.
/// MariaDB 11 answers a syntax error as `ERROR 1064 (42000)` with its own
/// wording; the errno and SQLSTATE already matched, the message body did
/// not have to carry SPG's parser internals.
#[test]
fn a_parse_error_carries_no_internal_prefix() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    let (errno, sqlstate, msg) = err_of(&mut s, "SELECT * FROM");
    assert_eq!((errno, sqlstate.as_str()), (1064, "42000"));
    assert!(
        !msg.contains("parse error at token"),
        "SPG's token index leaked: {msg}"
    );
    for prefix in ["parse: ", "eval: ", "unsupported: ", "storage: ", "lex: "] {
        assert!(
            !msg.starts_with(prefix),
            "SPG's internal class vocabulary leaked: {msg}"
        );
    }
    // Still usable.
    assert_eq!(query_scalar(&mut s, "SELECT 1"), "1");
}

/// The same for an error raised past the parser.
#[test]
fn a_runtime_error_carries_no_internal_prefix() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    let (_errno, _sqlstate, msg) = err_of(&mut s, "SELECT * FROM no_such_table");
    for prefix in ["parse: ", "eval: ", "unsupported: ", "storage: ", "lex: "] {
        assert!(
            !msg.starts_with(prefix),
            "SPG's internal class vocabulary leaked: {msg}"
        );
    }
}

/// V50 (round 331) — `SET autocommit=0`.
///
/// MariaDB 11 measured: with autocommit off the connection accumulates
/// changes until COMMIT or ROLLBACK — an INSERT is visible to the session
/// that made it, `ROLLBACK` discards it, `COMMIT` keeps it, and a
/// disconnect without either rolls back. `@@autocommit` reads 0, and the
/// OK packet's status flags drop the AUTOCOMMIT bit (measured in round
/// 316 with a native-protocol probe).
///
/// SPG ignored the setting: every statement committed on its own, so a
/// client that turned autocommit off and rolled back had already made its
/// writes permanent.
#[test]
fn set_autocommit_off_defers_the_commit() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE TABLE ac (id INT NOT NULL)");
    exec_ok(&mut s, "SET autocommit=0");

    exec_ok(&mut s, "INSERT INTO ac VALUES (1)");
    assert_eq!(
        query_scalar(&mut s, "SELECT COUNT(*) FROM ac"),
        "1",
        "the session sees its own uncommitted write"
    );
    exec_ok(&mut s, "ROLLBACK");
    assert_eq!(
        query_scalar(&mut s, "SELECT COUNT(*) FROM ac"),
        "0",
        "ROLLBACK discards it — with autocommit on it would already be permanent"
    );

    exec_ok(&mut s, "INSERT INTO ac VALUES (2)");
    exec_ok(&mut s, "COMMIT");
    assert_eq!(query_scalar(&mut s, "SELECT COUNT(*) FROM ac"), "1");
}

/// The flag is readable, as MariaDB's is.
#[test]
fn at_at_autocommit_reports_the_setting() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut s, "SELECT @@autocommit"), "1");
    exec_ok(&mut s, "SET autocommit=0");
    assert_eq!(query_scalar(&mut s, "SELECT @@autocommit"), "0");
    exec_ok(&mut s, "SET autocommit=1");
    assert_eq!(query_scalar(&mut s, "SELECT @@autocommit"), "1");
}

/// And the protocol's own status bit follows it (round 316 measured the
/// bit on MariaDB; this is the other half of that finding).
#[test]
fn the_status_flags_drop_autocommit_when_it_is_off() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(status_of_select(&mut s, "SELECT 1"), AUTOCOMMIT);
    assert_eq!(status_of(&mut s, "SET autocommit=0"), 0, "the bit is cleared");
    exec_ok(&mut s, "CREATE TABLE ac2 (id INT NOT NULL)");
    // Inside the implicit block the transaction bit is on and AUTOCOMMIT
    // stays off.
    assert_eq!(status_of(&mut s, "INSERT INTO ac2 VALUES (1)"), IN_TRANS);
    exec_ok(&mut s, "ROLLBACK");
    assert_eq!(status_of(&mut s, "SET autocommit=1"), AUTOCOMMIT);
}

/// A connection that goes away without committing loses the work.
#[test]
fn a_disconnect_under_autocommit_off_rolls_back() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    exec_ok(&mut a, "CREATE TABLE ac3 (id INT NOT NULL)");

    {
        let mut b = auth_open_mode(&addr);
        exec_ok(&mut b, "SET autocommit=0");
        exec_ok(&mut b, "INSERT INTO ac3 VALUES (1)");
        write_packet(&mut b, 0, &[0x01]); // COM_QUIT
    }
    // Give the server a moment to finish the disconnect cleanup.
    for _ in 0..100 {
        if query_scalar(&mut a, "SELECT COUNT(*) FROM ac3") == "0" {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("an uncommitted write survived the disconnect");
}
