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
    let p =
        crate::common::tmp_base().join(format!("spg-e2e-mysqlwire-query-{label}-{pid}-{nanos}"));
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

/// Like `send_query`, but hands back the write error instead of
/// panicking: a connection the server has already closed refuses the
/// write, and for a test about closure that is an answer, not a fault.
fn write_query(stream: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03); // COM_QUERY
    payload.extend_from_slice(sql.as_bytes());
    let len = u32::try_from(payload.len()).expect("query fits a packet");
    let hdr = [len as u8, (len >> 8) as u8, (len >> 16) as u8, 0u8];
    stream.write_all(&hdr)?;
    stream.write_all(&payload)?;
    Ok(())
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

/// v7.39.2 — a handshake that NAMES a database, which is the door the
/// name arrives through for an ordinary client and which
/// `build_handshake_response` could not open: it never set
/// CLIENT_CONNECT_WITH_DB (0x0008), so every test connected with none.
fn build_handshake_response_with_db(username: &str, db: &str) -> Vec<u8> {
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000 | 0x0000_0008;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.push(0); // auth_response empty → open mode accepts
    payload.extend_from_slice(db.as_bytes());
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

fn auth_open_mode_with_db(addr: &str, db: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _greeting) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_handshake_response_with_db("anyone", db));
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
    assert_eq!(
        status_of(&mut s, "ROLLBACK"),
        AUTOCOMMIT,
        "ROLLBACK clears it"
    );
    assert_eq!(
        status_of_select(&mut s, "SELECT 1"),
        AUTOCOMMIT,
        "idle again"
    );
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
    assert_eq!(
        ok_status(&pkt),
        AUTOCOMMIT | IN_TRANS,
        "ping inside a block"
    );
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
    let nul = 1 + greeting[1..]
        .iter()
        .position(|&b| b == 0)
        .expect("version NUL");
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

    assert_eq!(
        a_row[4].as_deref(),
        Some("Query"),
        "the asker is running one"
    );
    assert_eq!(
        a_row[7].as_deref(),
        Some("SHOW PROCESSLIST"),
        "its own Info is the statement it is running"
    );
    assert_eq!(
        b_row[4].as_deref(),
        Some("Sleep"),
        "B is between statements"
    );
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
    //
    // Either half may be the one to report it. A closed TCP connection
    // refuses the write with EPIPE if the close has already landed, and
    // refuses the read if it lands while the request is in flight; which
    // of the two happens is a question of how busy the machine is, not
    // of whether the connection is gone. Round 858's gate saw the write
    // side, twice, on a box under load, where the same test passes alone
    // and passes 540-for-540 twice through the whole e2e suite.
    //
    // So the assertion is that the exchange fails, not that it fails in
    // a particular half.
    let mut hdr = [0u8; 4];
    let exchange_failed =
        write_query(&mut victim, "SELECT 1").is_err() || victim.read_exact(&mut hdr).is_err();
    assert!(
        exchange_failed,
        "the killed connection must be closed, on write or on read"
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

    // The server has closed the socket, correctly. Which syscall
    // observes that is a race this test must not lose: the follow-up
    // write usually slips into the void and the read then sees EOF, but
    // under load the scheduler can stall this thread mid-`write_packet`
    // — between its header and payload writes — long enough for the
    // server's RST to land, and then the PAYLOAD write takes EPIPE.
    // `send_query` would panic on that, and did, exactly once per few
    // full-load suite runs; a 300ms stall between the two writes
    // reproduces it 3 in 3. Both outcomes are the closure this test
    // exists to observe, so both pass.
    {
        use std::io::Write;
        let payload = [0x03, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1'];
        let hdr = [payload.len() as u8, 0, 0, 0];
        let wrote = s.write_all(&hdr).and_then(|()| s.write_all(&payload));
        if wrote.is_ok() {
            let mut hdr = [0u8; 4];
            assert!(
                s.read_exact(&mut hdr).is_err(),
                "the connection must be closed after killing itself"
            );
        }
        // A write error IS the closed connection announcing itself.
    }
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
    assert_eq!(
        status_of(&mut s, "SET autocommit=0"),
        0,
        "the bit is cleared"
    );
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

/// v7.38.17 — the same four questions v7.38.16 answered wrongly, asked
/// over the wire a MySQL client actually speaks.
///
/// Those four were found with an in-process probe, and the corpus could
/// not ask them at all: `corpus/mysql/` ran in PostgreSQL dialect from
/// the day it was created. The corpus can ask now, but it asks
/// in-process, and this project has been caught twice building something
/// its probe could not reach. So the questions are asked here too, on
/// the protocol path, where the release gate runs them.
///
/// Every expectation is MySQL 9.7.1's own answer at its default
/// collation `utf8mb4_0900_ai_ci`, read from the oracle:
///
///   s = 'ALPHA'                       1
///   s IN ('ALPHA','BETA')             1,2
///   s BETWEEN 'ALPHA' AND 'DELTA'     1,2,4
///   ORDER BY s LIMIT 2                1,2
///   a JOIN b ON a.s = b.s             1/10, 2/20
///
/// An index must not change any of them. Before v7.38.16 the first two
/// returned NOTHING with an index present and the join returned the
/// empty set.
#[test]
fn an_index_does_not_change_the_answer_over_the_mysql_wire() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);

    let col = |rows: Vec<Vec<Option<String>>>| -> Vec<String> {
        rows.into_iter()
            .map(|r| r[0].clone().unwrap_or_else(|| "NULL".into()))
            .collect()
    };

    for stage in ["no index", "indexed"] {
        exec_ok(&mut s, "DROP TABLE IF EXISTS mw");
        exec_ok(&mut s, "CREATE TABLE mw (k INT, s TEXT)");
        exec_ok(
            &mut s,
            "INSERT INTO mw VALUES (1,'alpha'),(2,'Beta'),(3,'GAMMA'),(4,'delta')",
        );
        if stage == "indexed" {
            exec_ok(&mut s, "CREATE INDEX mw_s ON mw (s)");
        }

        assert_eq!(
            col(query_rows(&mut s, "SELECT k FROM mw WHERE s = 'ALPHA'")),
            vec!["1"],
            "{stage}: equality folds in MySQL; 9.7.1 answers 1"
        );
        assert_eq!(
            col(query_rows(
                &mut s,
                "SELECT k FROM mw WHERE s IN ('ALPHA','BETA') ORDER BY k"
            )),
            vec!["1", "2"],
            "{stage}: IN folds too; 9.7.1 answers 1,2"
        );
        assert_eq!(
            col(query_rows(
                &mut s,
                "SELECT k FROM mw WHERE s BETWEEN 'ALPHA' AND 'DELTA' ORDER BY k"
            )),
            vec!["1", "2", "4"],
            "{stage}: 9.7.1 answers 1,2,4"
        );
        assert_eq!(
            col(query_rows(&mut s, "SELECT k FROM mw ORDER BY s LIMIT 2")),
            vec!["1", "2"],
            "{stage}: ordering folds, so the byte order is the wrong order"
        );
    }

    // The join, which is where this defect took its worst shape: an
    // inner join returned no rows at all, and only when an index existed.
    for stage in ["no index", "indexed"] {
        exec_ok(&mut s, "DROP TABLE IF EXISTS ja");
        exec_ok(&mut s, "DROP TABLE IF EXISTS jb");
        exec_ok(&mut s, "CREATE TABLE ja (k INT, s TEXT)");
        exec_ok(&mut s, "CREATE TABLE jb (k INT, s TEXT)");
        exec_ok(&mut s, "INSERT INTO ja VALUES (1,'alpha'),(2,'Beta')");
        exec_ok(&mut s, "INSERT INTO jb VALUES (10,'ALPHA'),(20,'beta')");
        if stage == "indexed" {
            exec_ok(&mut s, "CREATE INDEX ja_s ON ja (s)");
            exec_ok(&mut s, "CREATE INDEX jb_s ON jb (s)");
        }
        let pairs: Vec<String> = query_rows(
            &mut s,
            "SELECT ja.k, jb.k FROM ja JOIN jb ON ja.s = jb.s ORDER BY ja.k",
        )
        .into_iter()
        .map(|r| format!("{}/{}", r[0].clone().unwrap(), r[1].clone().unwrap()))
        .collect();
        assert_eq!(
            pairs,
            vec!["1/10", "2/20"],
            "{stage}: 9.7.1 matches both pairs on a folding collation"
        );
    }

    exec_ok(&mut s, "DROP TABLE IF EXISTS mw");
    exec_ok(&mut s, "DROP TABLE IF EXISTS ja");
    exec_ok(&mut s, "DROP TABLE IF EXISTS jb");
}

/// v7.39 — a fresh MySQL connection is STRICT, and this has to be tested
/// on the wire because that is the only place it was ever wrong.
///
/// `SessionBag` derived `Default`, and `set_current_session` creates a
/// bag on first sight with `unwrap_or_default()`, so `mysql_strict`
/// started `false` on every new connection — while `@@sql_mode` answered
/// `STRICT_TRANS_TABLES`. Measured over the wire before the fix:
/// `VARCHAR(3) <- 'abcdef'` stored `'abc'` and `TINYINT <- 999` stored
/// `127`, silently, both reported as success. Data lost, with a receipt
/// saying the session was strict.
///
/// The whole engine-level suite — 6621 tests — could not see it. Those
/// build an `Engine` directly, where the field was already `true`; the
/// defect lived in the connection-switching path that only a real server
/// takes. A pin anywhere but here would have been green throughout.
///
/// The negative control is in the same test: `SET sql_mode = ''` must
/// still bend the value, because the fix is meant to correct the DEFAULT
/// and not to weld the switch shut. MariaDB 11 with an empty sql_mode
/// stores `'too'` for `VARCHAR(3) <- 'toolong'`, and so must this.
#[test]
fn a_fresh_connection_is_strict_and_can_still_be_told_not_to_be() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);

    exec_ok(
        &mut s,
        "CREATE TABLE strict_default (c VARCHAR(3), n TINYINT)",
    );

    // No SET of any kind on this connection first — that is the point.
    let (errno, sqlstate, _) = err_of(&mut s, "INSERT INTO strict_default (c) VALUES ('abcdef')");
    assert_eq!(
        (errno, sqlstate.as_str()),
        (1406, "22001"),
        "an over-long value must be refused on a connection that set nothing"
    );

    let (errno, _, _) = err_of(&mut s, "INSERT INTO strict_default (n) VALUES (999)");
    assert_ne!(errno, 0, "an out-of-range value must be refused too");

    // Nothing landed: the refusals are not cosmetic.
    assert_eq!(
        query_scalar(&mut s, "SELECT count(*) FROM strict_default"),
        "0"
    );

    // And the switch still switches.
    exec_ok(&mut s, "SET sql_mode = ''");
    exec_ok(&mut s, "INSERT INTO strict_default (c) VALUES ('abcdef')");
    assert_eq!(
        query_scalar(&mut s, "SELECT c FROM strict_default"),
        "abc",
        "with an empty sql_mode the value is bent to fit, as MariaDB does"
    );
}

/// v7.39 — every flag `@@sql_mode` claims must actually refuse something.
///
/// This pins the REPORT against the BEHAVIOUR, which is the failure this
/// version keeps meeting: `SHOW VARIABLES` named two flags, `@@sql_mode`
/// named three, and both named `NO_ENGINE_SUBSTITUTION` while
/// `ENGINE=NONSUCH` was accepted silently. A test that only compared the
/// two reports would have called that agreement.
///
/// Each statement below is refused by exactly one of the flags, so the
/// list cannot grow a member SPG does not honour without this going red.
/// Verified against MySQL 9.7.2, which refuses all four.
#[test]
fn every_flag_sql_mode_claims_refuses_something() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);

    let claimed = query_scalar(&mut s, "SELECT @@sql_mode");
    exec_ok(&mut s, "CREATE TABLE modes (c VARCHAR(3), d DATE, n INT)");

    for (flag, sql) in [
        (
            "STRICT_TRANS_TABLES",
            "INSERT INTO modes (c) VALUES ('abcdef')",
        ),
        (
            "NO_ZERO_DATE",
            "INSERT INTO modes (d) VALUES ('0000-00-00')",
        ),
        (
            "NO_ZERO_IN_DATE",
            "INSERT INTO modes (d) VALUES ('2020-00-05')",
        ),
        (
            "ERROR_FOR_DIVISION_BY_ZERO",
            "INSERT INTO modes (n) VALUES (1/0)",
        ),
    ] {
        assert!(
            claimed.contains(flag),
            "{flag} is enforced but not claimed — @@sql_mode says {claimed:?}"
        );
        let (errno, _, _) = err_of(&mut s, sql);
        assert_ne!(
            errno, 0,
            "`{sql}` must be refused, since @@sql_mode claims {flag}"
        );
    }

    // v7.39 — this assertion used to run the other way: the flag was off
    // the list, and the pin said so, so that implementing the check would
    // turn it red and act as the reminder to add the flag back. It did.
    assert!(
        claimed.contains("NO_ENGINE_SUBSTITUTION"),
        "NO_ENGINE_SUBSTITUTION is honoured but not claimed — @@sql_mode says {claimed:?}"
    );
    let (errno, sqlstate, msg) = err_of(&mut s, "CREATE TABLE eng_ns (a INT) ENGINE=NONSUCH");
    assert_eq!(
        (errno, sqlstate.as_str()),
        (1286, "42000"),
        "an engine MySQL does not know must be refused as MySQL refuses it"
    );
    assert!(
        msg.starts_with("Unknown storage engine"),
        "MySQL 9.7.2's own wording, got: {msg}"
    );
    // v7.39.3 — and the name AS WRITTEN. MySQL 9.7.2 quotes it back
    // exactly: `Unknown storage engine 'NoSuchEng'`. The lexer folds a
    // bare identifier, so the message quoted a name the dump did not
    // contain — which is the one thing this message is for, since the
    // reader's next move is to search their dump for it.
    let (_, _, mixed) = err_of(&mut s, "CREATE TABLE eng_mx (a INT) ENGINE=NoSuchEng");
    assert_eq!(mixed, "Unknown storage engine 'NoSuchEng'", "{mixed}");
    // Quoted, too — the quotes are not part of the name.
    let (_, _, quoted) = err_of(&mut s, "CREATE TABLE eng_q (a INT) ENGINE='NoSuchEng'");
    assert_eq!(quoted, "Unknown storage engine 'NoSuchEng'", "{quoted}");
    // The names in a real dump keep working — refusing those would be a
    // worse defect than accepting a typo, and a quieter one.
    // Numbered, not named after the engine: identifiers fold, so
    // `eng_InnoDB` and `eng_innodb` would be ONE table and the second
    // CREATE would fail for a reason with nothing to do with engines.
    for (i, known) in ["InnoDB", "innodb", "MyISAM", "MEMORY", "ARCHIVE"]
        .iter()
        .enumerate()
    {
        exec_ok(
            &mut s,
            &format!("CREATE TABLE eng_ok_{i} (a INT) ENGINE={known}"),
        );
    }
    // FEDERATED has a row in MySQL's own ENGINES table and is still
    // refused there, because that build cannot provide it. A list copied
    // from the table without reading `support` would accept it.
    let (errno, _, _) = err_of(&mut s, "CREATE TABLE eng_fed (a INT) ENGINE=FEDERATED");
    assert_eq!(
        errno, 1286,
        "FEDERATED is in the ENGINES table and still refused"
    );
    // v7.39.2 — the reverse pin that stood here said the opposite:
    // ONLY_FULL_GROUP_BY must be ABSENT from the claim while a bare
    // non-aggregated column was still allowed through. It is enforced
    // now, so the claim is honest and the pin holds both halves —
    // claimed AND kept, which is the only pair worth pinning.
    assert!(
        claimed.contains("ONLY_FULL_GROUP_BY"),
        "ONLY_FULL_GROUP_BY is enforced but not claimed — @@sql_mode says {claimed:?}"
    );
    exec_ok(&mut s, "CREATE TABLE ofgb (g INT, v INT)");
    exec_ok(&mut s, "INSERT INTO ofgb VALUES (1, 10), (1, 20)");
    let (errno, state, _) = err_of(&mut s, "SELECT g, v FROM ofgb GROUP BY g");
    assert_eq!(
        errno, 1055,
        "ER_WRONG_FIELD_WITH_GROUP, as MySQL 9.7.2 answers"
    );
    assert_eq!(state, "42000");

    // Nothing above landed.
    assert_eq!(query_scalar(&mut s, "SELECT count(*) FROM modes"), "0");
}

/// v7.39 — a value that will not fit is refused in MySQL's words, with
/// MySQL's errno and SQLSTATE.
///
/// The bend path had spoken MySQL since round 470 — `Out of range value
/// for column 'n' at row 1`, errno 1264 — and the REFUSAL path still
/// spoke PostgreSQL's: `integer out of range` with errno 1690, `value
/// too long for type character varying(3)`, and a numeric overflow that
/// carried its `DETAIL:` clause inline. The same failure, described two
/// ways, and the client had asked for one of them by connecting here.
///
/// Every pair below was read back from MySQL 9.7.2 rather than reasoned
/// about, and four of them are not what reasoning would have given:
///
///   * strict does NOT simply reuse the warning's code. A too-long
///     string warns 1265 `Data truncated` and refuses 1406 `Data too
///     long` — both the code and the wording change. Numerics keep 1264
///     for both.
///   * `'12xy'` into an INT is 1265 (it takes the leading digits);
///     into a DECIMAL it is 1366. Same input shape, different class.
///   * the noun follows the column type and MySQL is not consistent
///     about its case: `integer`, `decimal`, `FLOAT`, `DOUBLE`.
///   * a DECIMAL that overflows is 1264, not the 1265 a non-integer
///     column would otherwise get.
///
/// On the wire because the SQLSTATE only exists here: the engine returns
/// a message and this layer derives the pair, so an engine-level test
/// cannot see whether a client is told 1264 or a generic 1064.
#[test]
fn a_value_that_will_not_fit_is_refused_the_way_mysql_refuses_it() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);

    for (i, (ddl, value, errno, sqlstate, msg)) in [
        (
            "n TINYINT",
            "999",
            1264,
            "22003",
            "Out of range value for column 'n' at row 1",
        ),
        (
            "n INT",
            "99999999999",
            1264,
            "22003",
            "Out of range value for column 'n' at row 1",
        ),
        (
            "n INT UNSIGNED",
            "-5",
            1264,
            "22003",
            "Out of range value for column 'n' at row 1",
        ),
        (
            "c VARCHAR(3)",
            "'abcdef'",
            1406,
            "22001",
            "Data too long for column 'c' at row 1",
        ),
        (
            "c CHAR(3)",
            "'abcdef'",
            1406,
            "22001",
            "Data too long for column 'c' at row 1",
        ),
        (
            "d DECIMAL(3,1)",
            "9999",
            1264,
            "22003",
            "Out of range value for column 'd' at row 1",
        ),
    ]
    .iter()
    .enumerate()
    {
        exec_ok(&mut s, &format!("CREATE TABLE fit_{i} ({ddl})"));
        let (got_errno, got_state, got_msg) =
            err_of(&mut s, &format!("INSERT INTO fit_{i} VALUES ({value})"));
        assert_eq!(
            (got_errno, got_state.as_str()),
            (*errno, *sqlstate),
            "`{ddl} <- {value}`"
        );
        assert_eq!(got_msg, *msg, "`{ddl} <- {value}`");
    }
}

/// The other direction, and the one that caught a defect this change
/// introduced: a value that FITS must still be stored.
///
/// The DECIMAL clamp's first cut restated every value at the column's
/// scale and handed that back, so the classifier — which decides "did
/// not fit" by comparing before with after — read ordinary rounding as
/// an overflow. A strict session then REFUSED `DECIMAL(3,1) <- 1.26`,
/// which MySQL stores as `1.3`. All six refusal probes were green while
/// that was true.
#[test]
fn a_value_that_fits_is_still_stored() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);

    for (i, (ddl, value, stored)) in [
        ("n TINYINT", "127", "127"),
        ("c VARCHAR(3)", "'abc'", "abc"),
        ("d DECIMAL(3,1)", "99.9", "99.9"),
        // Rounding to the declared scale is not an overflow. It happens
        // in strict mode too, on both engines.
        ("d DECIMAL(3,1)", "1.26", "1.3"),
        ("d DECIMAL(3,1)", "-1.26", "-1.3"),
    ]
    .iter()
    .enumerate()
    {
        exec_ok(&mut s, &format!("CREATE TABLE fits_{i} ({ddl})"));
        exec_ok(&mut s, &format!("INSERT INTO fits_{i} VALUES ({value})"));
        let col = ddl.split(' ').next().unwrap();
        assert_eq!(
            query_scalar(&mut s, &format!("SELECT {col} FROM fits_{i}")),
            *stored,
            "`{ddl} <- {value}` must be stored, not refused"
        );
    }
}

/// And a NON-strict session still bends rather than refusing — the fix
/// corrects what strict SAYS, it does not make the engine strict always.
/// The DECIMAL rows are new capability: SPG used to raise here, where
/// MySQL stores the bound, so a bulk load into a non-strict session
/// stopped on a row MySQL would have taken.
#[test]
fn a_non_strict_session_still_bends_the_value() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "SET sql_mode = ''");

    for (i, (ddl, value, bent)) in [
        ("n TINYINT", "999", "127"),
        ("c VARCHAR(3)", "'abcdef'", "abc"),
        ("d DECIMAL(3,1)", "9999", "99.9"),
        ("d DECIMAL(3,1)", "-9999", "-99.9"),
    ]
    .iter()
    .enumerate()
    {
        exec_ok(&mut s, &format!("CREATE TABLE bend_{i} ({ddl})"));
        exec_ok(&mut s, &format!("INSERT INTO bend_{i} VALUES ({value})"));
        let col = ddl.split(' ').next().unwrap();
        assert_eq!(
            query_scalar(&mut s, &format!("SELECT {col} FROM bend_{i}")),
            *bent,
            "`{ddl} <- {value}` must bend in a non-strict session"
        );
    }
}

/// v7.39 — in a MySQL session `"…"` opens a STRING, not an identifier.
///
/// SPG behaved as though `ANSI_QUOTES` were always in `sql_mode`, which
/// is PostgreSQL's rule applied to a MySQL client. `SELECT "abc"`
/// answered `ERROR 1054 column "abc" does not exist` where MySQL 9.7.2
/// answers `abc` — ordinary MySQL SQL failing with an error that names a
/// column its author never wrote.
///
/// Every expectation here was read off MySQL 9.7.2.
#[test]
fn a_double_quoted_string_is_a_string_in_a_mysql_session() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut s, r#"SELECT "abc""#), "abc");
    // Doubling the quote, and escaping it, both give one quote.
    assert_eq!(query_scalar(&mut s, r#"SELECT "a""b""#), "a\"b");
    assert_eq!(query_scalar(&mut s, r#"SELECT "a\"b""#), "a\"b");
    // The OTHER quote is an ordinary character inside.
    assert_eq!(query_scalar(&mut s, r#"SELECT "a'b""#), "a'b");
    // And it is a string wherever a string goes, not just in a select
    // list — the shape a WHERE clause written for MySQL takes.
    assert_eq!(query_scalar(&mut s, r#"SELECT 1 WHERE "x" = "x""#), "1");
}

/// The escapes flag and the quoting rule are separate questions.
///
/// v7.39 — `in_mysql_dialect()` IS `backslash_escapes`, so
/// `SET sql_mode='NO_BACKSLASH_ESCAPES'` used to stop the session being
/// MySQL by that test. Harmless until something else asked, and `"…"`
/// asks: the session got PostgreSQL's identifier rule back and
/// `SELECT LENGTH("\n")` failed on a column named newline.
#[test]
fn turning_escapes_off_does_not_turn_the_quoting_rule_back() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut s, r#"SELECT LENGTH("\n")"#), "1");
    exec_ok(&mut s, "SET sql_mode='NO_BACKSLASH_ESCAPES'");
    // Two bytes now — and still a STRING, which is the half that broke.
    assert_eq!(query_scalar(&mut s, r#"SELECT LENGTH("\n")"#), "2");
    assert_eq!(query_scalar(&mut s, r#"SELECT "abc""#), "abc");
}

/// `ANSI_QUOTES` turns the rule back on, and only that rule.
#[test]
fn ansi_quotes_makes_the_double_quote_an_identifier_again() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "SET sql_mode='ANSI_QUOTES'");
    // An identifier now, so this names a column that does not exist.
    send_query(&mut s, r#"SELECT "abc""#);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff, "expected an error packet");
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1054, "ER_BAD_FIELD_ERROR, as MySQL 9.7.2 answers");
    assert_eq!(&err[4..9], b"42S22");
    // Usable as one, too.
    assert_eq!(query_scalar(&mut s, r#"SELECT 1 AS "x""#), "1");
    // Single quotes are untouched by ANSI_QUOTES.
    assert_eq!(query_scalar(&mut s, "SELECT 'abc'"), "abc");
    // And dropping ANSI_QUOTES puts the string rule back.
    exec_ok(&mut s, "SET sql_mode=''");
    assert_eq!(query_scalar(&mut s, r#"SELECT "abc""#), "abc");
}

/// The quoting rule is per connection, like every other dialect flag.
#[test]
fn ansi_quotes_does_not_leak_between_connections() {
    let (_guard, addr) = spawn();
    let mut a = auth_open_mode(&addr);
    exec_ok(&mut a, "SET sql_mode='ANSI_QUOTES'");
    let mut b = auth_open_mode(&addr);
    assert_eq!(
        query_scalar(&mut b, r#"SELECT "abc""#),
        "abc",
        "a second connection starts from MySQL's default sql_mode"
    );
    // And A is unchanged by B having connected.
    send_query(&mut a, r#"SELECT "abc""#);
    let (_seq, err) = read_packet(&mut a);
    assert_eq!(err[0], 0xff, "A still has ANSI_QUOTES");
}

/// Backtick identifiers are unaffected either way — they are how
/// `mysqldump` writes every name.
#[test]
fn backtick_identifiers_are_unaffected_by_the_quoting_rule() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut s, "SELECT 1 AS `bt`"), "1");
    exec_ok(&mut s, "SET sql_mode='ANSI_QUOTES'");
    assert_eq!(query_scalar(&mut s, "SELECT 1 AS `bt`"), "1");
}

/// v7.39.2 — a binary string goes out as its bytes.
///
/// MySQL's `X'41'`, `x'41'`, `0x41` and `b'1000001'` are all the byte
/// 0x41, and MySQL 9.7.2 prints every one of them as `A`. SPG routed
/// them through the engine's canonical rendering, which is PostgreSQL's
/// `\x41` hex form — measured on all four spellings, and on the same
/// value reached through `COALESCE` and through a `UNION`.
///
/// This is a wire test because the divergence is in the wire encoder:
/// the engine's own `value_to_text` is PG's, correctly, and the MySQL
/// text row is where the two renderings part.
#[test]
fn a_binary_string_prints_as_its_bytes() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    for sql in [
        "SELECT X'41'",
        "SELECT x'41'",
        "SELECT 0x41",
        "SELECT b'1000001'",
        "SELECT COALESCE(X'41','z')",
    ] {
        assert_eq!(query_scalar(&mut s, sql), "A", "{sql}");
    }
    // Two bytes, and a byte that is not printable ASCII on its own,
    // so the encoder cannot be passing through a String round-trip.
    assert_eq!(query_scalar(&mut s, "SELECT CONCAT(X'41',X'42')"), "AB");
    assert_eq!(query_scalar(&mut s, "SELECT LENGTH(X'00FF')"), "2");
    // v7.39.2 — and `NO_BACKSLASH_ESCAPES` does not take the literal
    // away with the escapes. `0x41` is lexed as a NUMBER when the
    // session is not MySQL, so this answered 65 while the same
    // connection still called itself MySQL.
    exec_ok(&mut s, "SET sql_mode='NO_BACKSLASH_ESCAPES'");
    assert_eq!(query_scalar(&mut s, "SELECT 0x41"), "A");
    assert_eq!(query_scalar(&mut s, "SELECT X'41'"), "A");
    assert_eq!(query_scalar(&mut s, "SELECT 7 DIV 2"), "3");
    assert_eq!(query_scalar(&mut s, "SELECT 1 # a hash comment"), "1");
    // MySQL's block comments do not nest, so the FIRST `*/` closes this
    // one and the `1` is the whole statement. PG nests, and would still
    // be looking for a closer. Measured on both.
    assert_eq!(query_scalar(&mut s, "SELECT /* a /* b */ 1"), "1");
}

/// v7.39.2 — `GROUP_CONCAT` over binary strings answers their bytes.
/// It refused them outright ("string_agg requires text value, got
/// bytea") until the aggregate learned PG18's bytea form.
#[test]
fn group_concat_over_binary_strings_prints_its_bytes() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    assert_eq!(
        query_scalar(
            &mut s,
            "SELECT GROUP_CONCAT(v ORDER BY v) FROM (SELECT X'42' v UNION ALL SELECT X'41') z"
        ),
        "A,B"
    );
}

/// v7.39.2 — an unknown collation carries MySQL's errno, not 1064.
///
/// A driver branches on the number. Measured on MySQL 9.7.2:
/// `ERROR 1273 (HY000): Unknown collation: 'nosuch_ci'`.
#[test]
fn an_unknown_collation_is_1273() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // Through an EXPRESSION and through a DECLARATION — two different
    // engine errors, and the errno has to be the same from both.
    let (errno, state, msg) = err_of(&mut s, "SELECT 'a' COLLATE nosuch_ci");
    assert_eq!((errno, state.as_str()), (1273, "HY000"), "{msg}");
    assert_eq!(msg, "Unknown collation: 'nosuch_ci'");
    let (errno, state, msg) = err_of(&mut s, "CREATE TABLE uc (s VARCHAR(8) COLLATE nosuch_ci)");
    assert_eq!((errno, state.as_str()), (1273, "HY000"), "{msg}");
    assert_eq!(msg, "Unknown collation: 'nosuch_ci'");
    // The control: a name MySQL does have is not refused.
    assert_eq!(query_scalar(&mut s, "SELECT 'a' COLLATE utf8mb4_bin"), "a");
}

/// v7.39.2 — three failures that were all reported as 1064, a PARSE
/// error, which is not what happened in any of them. A driver branches
/// on the number: 1064 sends a caller down "the statement is malformed"
/// when the statement parsed perfectly well.
///
/// Every pair measured against MySQL 9.7.2.
#[test]
fn three_failures_carry_their_own_errno() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // 1136 / 21S01, one sentence for both directions — MySQL's own,
    // where SPG carries PostgreSQL's two on the other wire.
    exec_ok(&mut s, "CREATE TABLE ec (a INT)");
    let (errno, state, msg) = err_of(&mut s, "INSERT INTO ec VALUES (1,2)");
    assert_eq!((errno, state.as_str()), (1136, "21S01"), "{msg}");
    assert_eq!(msg, "Column count doesn't match value count at row 1");
    exec_ok(&mut s, "CREATE TABLE ed (a INT, b INT)");
    let (errno, state, msg) = err_of(&mut s, "INSERT INTO ed (a,b) VALUES (1)");
    assert_eq!((errno, state.as_str()), (1136, "21S01"), "{msg}");
    assert_eq!(msg, "Column count doesn't match value count at row 1");

    // 1193 / HY000. The sentence was already MySQL's; only the number
    // was wrong.
    let (errno, state, msg) = err_of(&mut s, "SELECT @@nosuchvar");
    assert_eq!((errno, state.as_str()), (1193, "HY000"), "{msg}");
    assert_eq!(msg, "Unknown system variable 'nosuchvar'");

    // 1305 / 42000. SPG keeps its own sentence, which names the argument
    // types and so says more about why nothing matched.
    let (errno, state, msg) = err_of(&mut s, "SELECT nosuchfunc(1)");
    assert_eq!((errno, state.as_str()), (1305, "42000"), "{msg}");
    assert!(msg.contains("nosuchfunc"), "{msg}");

    // The control: a statement that really IS malformed still says so.
    let (errno, _state, msg) = err_of(&mut s, "SELECT FROM");
    assert_eq!(errno, 1064, "{msg}");
}

/// v7.39.2 — `DATABASE()` on the wire names the connection's database.
///
/// It answered the constant `spg` in all three of MySQL's states. This
/// is the wire half: the handshake name arrives here and nowhere else,
/// and a client that switches with COM_INIT_DB rather than a `USE`
/// query goes through a different door again.
#[test]
fn the_wire_names_the_connections_database() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    // `auth_open_mode` sends no database, which is MySQL's NULL state.
    assert_eq!(query_scalar(&mut s, "SELECT DATABASE()"), "");
    exec_ok(&mut s, "USE myapp");
    assert_eq!(query_scalar(&mut s, "SELECT DATABASE()"), "myapp");
    // Per connection, not per server: a second one starts from NULL.
    let mut b = auth_open_mode(&addr);
    assert_eq!(query_scalar(&mut b, "SELECT DATABASE()"), "");
    // And A is unchanged by B having connected.
    assert_eq!(query_scalar(&mut s, "SELECT DATABASE()"), "myapp");
    // The third door: the name given at HANDSHAKE, which no test could
    // reach until this file learned to set CLIENT_CONNECT_WITH_DB.
    let mut c = auth_open_mode_with_db(&addr, "fromhandshake");
    assert_eq!(query_scalar(&mut c, "SELECT DATABASE()"), "fromhandshake");
    // And the fourth: COM_INIT_DB, which is what a driver's
    // `mysql_select_db()` and a pool's connection reset send. The `mysql`
    // CLI sends `USE x` as a QUERY, so no test reaches this by writing
    // SQL — reverting the wiring behind it left every other assertion
    // here green.
    let mut payload = vec![0x02u8];
    payload.extend_from_slice(b"viainitdb");
    write_packet(&mut c, 0, &payload);
    let (_seq, ok) = read_packet(&mut c);
    assert_eq!(ok[0], 0x00, "expected OK for COM_INIT_DB, got {:#x}", ok[0]);
    assert_eq!(query_scalar(&mut c, "SELECT DATABASE()"), "viainitdb");
}

/// v7.39.2 — an unknown column says which CLAUSE, the way MySQL does.
///
/// SPG carried PostgreSQL's clause-free `column "x" does not exist` to
/// the MySQL wire. MySQL 9.7.2 names the clause in six different ways
/// and a driver's error handling reads the sentence as well as the
/// number. Every expectation below is measured against 9.7.2.
///
/// The clause is known only in the SELECT validator — by the time a
/// row-time resolver meets the name, the expression has been detached
/// from the statement that held it — so the shapes that validator
/// declines fall back to `'field list'`, which is what the projection,
/// an UPDATE's SET list and an INSERT's target columns all say anyway.
#[test]
fn an_unknown_column_names_its_clause() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE TABLE uc (a INT)");
    exec_ok(&mut s, "CREATE TABLE u2 (b INT)");
    for (sql, want) in [
        (
            "SELECT nosuch FROM uc",
            "Unknown column 'nosuch' in 'field list'",
        ),
        (
            "UPDATE uc SET nosuch = 1",
            "Unknown column 'nosuch' in 'field list'",
        ),
        (
            "INSERT INTO uc (nosuch) VALUES (1)",
            "Unknown column 'nosuch' in 'field list'",
        ),
        (
            "SELECT a FROM uc WHERE nosuch = 1",
            "Unknown column 'nosuch' in 'where clause'",
        ),
        (
            "SELECT a FROM uc ORDER BY nosuch",
            "Unknown column 'nosuch' in 'order clause'",
        ),
        (
            "SELECT a FROM uc GROUP BY nosuch",
            "Unknown column 'nosuch' in 'group statement'",
        ),
        (
            "SELECT a FROM uc GROUP BY a HAVING nosuch > 1",
            "Unknown column 'nosuch' in 'having clause'",
        ),
        (
            "SELECT a FROM uc JOIN u2 ON uc.nosuch = u2.b",
            "Unknown column 'uc.nosuch' in 'on clause'",
        ),
        // A join keeps the clause too — the validator reads every source,
        // not just the first.
        (
            "SELECT a FROM uc JOIN u2 ON uc.a = u2.b WHERE nosuch = 1",
            "Unknown column 'nosuch' in 'where clause'",
        ),
        // The qualifier travels with the name.
        (
            "SELECT a FROM uc x WHERE x.nosuch = 1",
            "Unknown column 'x.nosuch' in 'where clause'",
        ),
        // A qualifier that names no source at all. PostgreSQL calls this
        // a missing FROM-clause entry and SPG numbered it 1146, which a
        // driver reads as "that table is gone"; MySQL calls it a COLUMN.
        (
            "SELECT a FROM uc WHERE zz.a = 1",
            "Unknown column 'zz.a' in 'where clause'",
        ),
    ] {
        let (errno, state, msg) = err_of(&mut s, sql);
        assert_eq!((errno, state.as_str()), (1054, "42S22"), "{sql}: {msg}");
        assert_eq!(msg, want, "{sql}");
    }

    // `q.*` is a TABLE error on MySQL, not a column one, and carries a
    // different number: pinning only the row above would have let the
    // star share it.
    let (errno, state, msg) = err_of(&mut s, "SELECT zz.* FROM uc");
    assert_eq!((errno, state.as_str()), (1051, "42S02"), "{msg}");
    assert_eq!(msg, "Unknown table 'zz'");

    // A name that matches two sources.
    exec_ok(&mut s, "CREATE TABLE u3 (a INT)");
    let (errno, state, msg) = err_of(&mut s, "SELECT a FROM uc JOIN u3 ON uc.a = u3.a");
    assert_eq!((errno, state.as_str()), (1052, "23000"), "{msg}");
    assert_eq!(msg, "Column 'a' in field list is ambiguous");

    // The controls: a real syntax error is still 1064, and a column that
    // does resolve still runs.
    assert_eq!(err_of(&mut s, "SELECT FROM").0, 1064);
    assert_eq!(query_scalar(&mut s, "SELECT COUNT(*) FROM uc"), "0");
}

/// v7.39.2 — a float renders MySQL's way ON THE WIRE.
///
/// The wire had its own copy of this: Rust's `Display` for f64 and
/// PostgreSQL's `float4out` for f32, in `value_to_mysql_text`, so the
/// engine's MySQL rule could not reach it however the session was
/// configured. The ablation that put that copy back left every engine
/// pin green, which is what said this surface had none of its own.
///
/// Measured on MySQL 9.7.2: a FLOAT prints to six significant digits,
/// and both widths stay in fixed notation for decimal exponents in
/// `[-15, 14]`.
#[test]
fn a_float_renders_mysqls_way_over_the_wire() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE TABLE wf (f FLOAT, d DOUBLE)");
    for (input, want_float, want_double) in [
        ("3.14159265358979", "3.14159", "3.14159265358979"),
        ("123456789", "123457000", "123456789"),
        ("1e14", "100000000000000", "100000000000000"),
        ("1e15", "1e15", "1e15"),
        ("1e-16", "1e-16", "1e-16"),
        ("0.1", "0.1", "0.1"),
        ("1.0", "1", "1"),
    ] {
        exec_ok(&mut s, "DELETE FROM wf");
        exec_ok(&mut s, &format!("INSERT INTO wf VALUES ({input}, {input})"));
        assert_eq!(
            query_scalar(&mut s, "SELECT f FROM wf"),
            want_float,
            "float {input}"
        );
        assert_eq!(
            query_scalar(&mut s, "SELECT d FROM wf"),
            want_double,
            "double {input}"
        );
    }
}

/// v7.39.2 — MySQL's own ONLY_FULL_GROUP_BY sentences, and its two
/// DIFFERENT errnos.
///
/// SPG enforced the rule (it is PostgreSQL's, and the same rule) but
/// answered PostgreSQL's one sentence and errno 1055 to every shape.
/// Measured on MySQL 9.7.2 there are four distinct answers, and two of
/// them are not 1055 at all — a driver branching on the number could
/// not tell an aggregated-query-without-GROUP-BY from an ungrouped
/// select-list column, and read a HAVING scope error as either.
#[test]
fn only_full_group_by_speaks_mysqls_words_and_numbers() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE DATABASE testdb");
    exec_ok(&mut s, "USE testdb");
    exec_ok(&mut s, "CREATE TABLE g (a INT, b INT, c INT)");
    exec_ok(
        &mut s,
        "INSERT INTO g VALUES (1,10,100),(1,20,200),(2,30,300)",
    );

    let ungrouped = |what: &str, col: &str| {
        format!(
            "Expression #{} is not in GROUP BY clause and contains nonaggregated \
             column '{col}' which is not functionally dependent on columns in GROUP \
             BY clause; this is incompatible with sql_mode=only_full_group_by",
            what
        )
    };

    let (errno, state, msg) = err_of(&mut s, "SELECT a, b FROM g GROUP BY a");
    assert_eq!((errno, state.as_str()), (1055, "42000"), "{msg}");
    assert_eq!(msg, ungrouped("2 of SELECT list", "testdb.g.b"));

    let (errno, state, msg) = err_of(&mut s, "SELECT a FROM g GROUP BY a ORDER BY b");
    assert_eq!((errno, state.as_str()), (1055, "42000"), "{msg}");
    assert_eq!(msg, ungrouped("1 of ORDER BY clause", "testdb.g.b"));

    // A different fault, and MySQL numbers it differently: 1140,
    // ER_MIX_OF_GROUP_FUNC_AND_FIELDS.
    let (errno, state, msg) = err_of(&mut s, "SELECT a, COUNT(*) FROM g");
    assert_eq!((errno, state.as_str()), (1140, "42000"), "{msg}");
    assert_eq!(
        msg,
        "In aggregated query without GROUP BY, expression #1 of SELECT list contains \
         nonaggregated column 'testdb.g.a'; this is incompatible with \
         sql_mode=only_full_group_by"
    );

    // And HAVING is not this error there at all: a grouped query's
    // HAVING sees only the grouped columns and the aggregates.
    let (errno, state, msg) = err_of(&mut s, "SELECT a FROM g GROUP BY a HAVING b > 1");
    assert_eq!((errno, state.as_str()), (1054, "42S22"), "{msg}");
    assert_eq!(msg, "Unknown column 'b' in 'having clause'");

    // The schema name follows the session, as every other
    // `db.table.column` on this wire does.
    exec_ok(&mut s, "USE other");
    let (_e, _st, msg) = err_of(&mut s, "SELECT a, b FROM g GROUP BY a");
    assert!(msg.contains("'other.g.b'"), "{msg}");

    // The control: turning the mode off runs the loose query.
    exec_ok(&mut s, "SET sql_mode=''");
    assert_eq!(
        // Two groups, so this is not a scalar without the LIMIT — the
        // helper reads exactly one row.
        query_scalar(&mut s, "SELECT b FROM g GROUP BY a ORDER BY a LIMIT 1"),
        "10"
    );
}

/// v7.39.2 — MySQL's wrong-parameter-count sentence and its OWN errno.
///
/// SPG said `lower() takes 1 arg, got 0` at 339 sites. MySQL 9.7.2 says
/// `Incorrect parameter count in the call to native function 'LOWER'`
/// and numbers it 1582 — a DIFFERENT number from 1305, which is what it
/// gives a name it does not know. PostgreSQL words both the same, so
/// only a distinct error variant can keep them apart on this wire.
#[test]
fn wrong_arity_counts_the_parameters() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(&mut s, "CREATE DATABASE testdb");
    exec_ok(&mut s, "USE testdb");
    for sql in ["SELECT LOWER()", "SELECT LOWER('a','b')"] {
        let (errno, state, msg) = err_of(&mut s, sql);
        assert_eq!((errno, state.as_str()), (1582, "42000"), "{sql}: {msg}");
        assert_eq!(
            msg, "Incorrect parameter count in the call to native function 'LOWER'",
            "{sql}"
        );
    }
    // The other number, and MySQL's own wording for it: schema-qualified,
    // upper-case FUNCTION, no argument types.
    let (errno, state, msg) = err_of(&mut s, "SELECT nosuchfn(1)");
    assert_eq!((errno, state.as_str()), (1305, "42000"), "{msg}");
    assert_eq!(msg, "FUNCTION testdb.nosuchfn does not exist");
    // It follows the session, like every other qualified name here.
    exec_ok(&mut s, "USE other");
    assert_eq!(
        err_of(&mut s, "SELECT nosuchfn(1)").2,
        "FUNCTION other.nosuchfn does not exist"
    );
    // The control: the right arity still runs.
    assert_eq!(query_scalar(&mut s, "SELECT LOWER('AB')"), "ab");
}

/// v7.39.2 — MySQL writes an explicit `NULL` for a nullable TIMESTAMP.
///
/// TIMESTAMP is NOT NULL by default there, so the word is what says
/// otherwise: `timestamp NULL DEFAULT NULL` (measured on 9.7.2), where
/// DATETIME — nullable by default — prints nothing. SPG wrote the
/// DATETIME form for both, so replaying its own dump made the column
/// NOT NULL.
#[test]
fn a_nullable_timestamp_says_null_out_loud() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    exec_ok(
        &mut s,
        "CREATE TABLE tn (a TIMESTAMP NULL, b DATETIME, c TIMESTAMP NOT NULL, \
         d TIMESTAMP(3) NULL)",
    );
    let ddl = query_rows(&mut s, "SHOW CREATE TABLE tn")[0][1]
        .clone()
        .expect("ddl");
    for want in [
        "`a` timestamp NULL DEFAULT NULL",
        "`b` datetime DEFAULT NULL",
        "`c` timestamp NOT NULL",
        "`d` timestamp(3) NULL DEFAULT NULL",
    ] {
        assert!(ddl.contains(want), "missing {want}:\n{ddl}");
    }
}

/// v7.39.3 — MySQL's own sentence for a syntax error.
///
/// Measured on 9.7.2, one sentence with two variables:
///
///     You have an error in your SQL syntax; check the manual that
///     corresponds to your MySQL server version for the right syntax to
///     use near '<the rest of the statement>' at line <N>
///
/// SPG answered `syntax error at or near "x"` with 1064 stapled on —
/// PostgreSQL's shape wearing MySQL's number. A client or a migration
/// tool matching what MySQL says found nothing, and this is the errno
/// every unrecognised statement lands on, so it is the sentence a MySQL
/// user sees most often.
///
/// The snippet runs from the failing token to the end, cut at 80
/// characters (measured with a padded alias), and the terminator is not
/// part of it.
#[test]
fn a_syntax_error_says_what_mysql_says() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    let head = "You have an error in your SQL syntax; check the manual that \
                corresponds to your MySQL server version for the right syntax \
                to use near ";
    // A table alias written as a STRING. MySQL: near '"al"' at line 1.
    let (errno, state, msg) = err_of(&mut s, "SELECT 1 FROM (SELECT 1) \"al\"");
    assert_eq!((errno, state.as_str()), (1064, "42000"), "{msg}");
    assert_eq!(msg, format!("{head}'\"al\"' at line 1"), "{msg}");
    // The line number is the token's, not the statement's first.
    let (_, _, msg) = err_of(&mut s, "SELECT 1\nFROM (SELECT 1) \"al\"");
    assert_eq!(msg, format!("{head}'\"al\"' at line 2"), "{msg}");
    // The terminator is not carried into the snippet.
    let (_, _, msg) = err_of(&mut s, "SELECT 1 FROM (SELECT 1) \"al\";");
    assert_eq!(msg, format!("{head}'\"al\"' at line 1"), "{msg}");
    // Cut at 80 characters, measured against 9.7.2 with a padded alias.
    let pad = "aliaslongenough_padding_padding_padding_padding_padding_padding_\
               padding_padding_padding_END";
    let (_, _, msg) = err_of(&mut s, &format!("SELECT 1 FROM (SELECT 1) \"{pad}\""));
    let near = msg
        .rsplit_once("near '")
        .and_then(|(_, t)| t.rsplit_once("' at line"))
        .map(|(t, _)| t.to_string())
        .unwrap_or_default();
    assert_eq!(near.chars().count(), 80, "{msg}");
    // An odd-digit hex literal, the other ledger line that turns out to
    // be this same sentence. MySQL 9.7.2: `near 'x'123'' at line 1`,
    // and with more of the statement after it, `near 'x'123', 2'`.
    let (errno, _, msg) = err_of(&mut s, "SELECT x'123'");
    assert_eq!(errno, 1064, "{msg}");
    assert_eq!(msg, format!("{head}'x'123'' at line 1"), "{msg}");
    let (_, _, msg) = err_of(&mut s, "SELECT 1, x'123', 2");
    assert_eq!(msg, format!("{head}'x'123', 2' at line 1"), "{msg}");
    // The control: a statement that parses is not made into an error by
    // any of this, and an EVEN-digit hex literal is a value.
    assert_eq!(query_scalar(&mut s, "SELECT 1 FROM (SELECT 1) t"), "1");
    assert_eq!(query_scalar(&mut s, "SELECT HEX(x'1234')"), "1234");
}
