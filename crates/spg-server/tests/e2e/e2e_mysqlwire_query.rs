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
    let (_seq, row) = read_packet(&mut s);
    let (value, _) = read_lenenc_string(&row, 0);
    assert_eq!(value, b"42");
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "trailing OK");
}

#[test]
fn select_text_literal_round_trips_through_lenenc_string() {
    let (_guard, addr) = spawn();
    let mut s = auth_open_mode(&addr);
    send_query(&mut s, "SELECT 'hello, mysql' AS greeting");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    let (_seq, row) = read_packet(&mut s);
    let (value, _) = read_lenenc_string(&row, 0);
    assert_eq!(value, b"hello, mysql");
    let (_seq, _ok) = read_packet(&mut s);
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
    let mut got_rows: Vec<(String, String)> = Vec::new();
    loop {
        let (_seq, pkt) = read_packet(&mut s);
        if pkt[0] == 0x00 {
            // trailing OK
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
    let (_seq, row1) = read_packet(&mut s);
    // row1 ordered "x" first (NULL b sorts later). row1 should
    // be NULL(0xfb) + 'x'.
    assert_eq!(row1[0], 0xfb, "NULL int column");
    let (b1, _) = read_lenenc_string(&row1, 1);
    assert_eq!(b1, b"x");
    let (_seq, row2) = read_packet(&mut s);
    let (a2, c) = read_lenenc_string(&row2, 0);
    assert_eq!(a2, b"1");
    assert_eq!(row2[c], 0xfb, "NULL text column");
    let (_seq, _ok) = read_packet(&mut s);
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
