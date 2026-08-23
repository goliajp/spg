//! v7.17.0 Phase 3.P0-74 — COM_STMT_PREPARE / EXECUTE / RESET /
//! CLOSE binary-protocol round-trip.
//!
//! Six angles:
//!   1. PREPARE returns StmtPrepareOk with id + num_columns +
//!      num_params + column_def_41 packets.
//!   2. EXECUTE with i32 / i64 / text params runs end-to-end.
//!   3. CLOSE removes the entry; a subsequent EXECUTE with that
//!      id returns 1243 (unknown handler).
//!   4. RESET replies with OK.
//!   5. Parse error in PREPARE returns 1064.
//!   6. EXECUTE with bad stmt id returns 1243.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-mysqlwire-stmt-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_packet(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).unwrap();
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let seqno = hdr[3];
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload).unwrap();
    (seqno, payload)
}

fn write_packet(s: &mut TcpStream, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    let hdr = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seqno];
    s.write_all(&hdr).unwrap();
    s.write_all(payload).unwrap();
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
    assert_eq!(ok[0], 0x00);
    s
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

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut p = vec![0x03];
    p.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &p);
}

fn send_prepare(s: &mut TcpStream, sql: &str) {
    let mut p = vec![0x16];
    p.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &p);
}

/// Decode StmtPrepareOk and consume all the column_def_41 /
/// trailing OK packets that follow. Returns the stmt id and the
/// (num_columns, num_params).
fn read_prepare_ok(s: &mut TcpStream) -> (u32, u16, u16) {
    let (_seq, payload) = read_packet(s);
    assert_eq!(payload[0], 0x00, "expected StmtPrepareOk header");
    let stmt_id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
    let num_columns = u16::from_le_bytes(payload[5..7].try_into().unwrap());
    let num_params = u16::from_le_bytes(payload[7..9].try_into().unwrap());
    // Drain parameter defs + OK
    if num_params > 0 {
        for _ in 0..num_params {
            let _ = read_packet(s);
        }
        let _ = read_packet(s); // trailing OK
    }
    if num_columns > 0 {
        for _ in 0..num_columns {
            let _ = read_packet(s);
        }
        let _ = read_packet(s); // trailing OK
    }
    (stmt_id, num_columns, num_params)
}

fn read_lenenc(buf: &[u8], pos: usize) -> (u64, usize) {
    let first = buf[pos];
    match first {
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

fn read_lenenc_string(buf: &[u8], pos: usize) -> (Vec<u8>, usize) {
    let (n, c) = read_lenenc(buf, pos);
    (buf[pos + c..pos + c + n as usize].to_vec(), c + n as usize)
}

/// Build a COM_STMT_EXECUTE payload with one INT param.
fn build_execute_int(stmt_id: u32, value: i32) -> Vec<u8> {
    let mut p = vec![0x17];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    p.push(0); // flags
    p.extend_from_slice(&1u32.to_le_bytes()); // iteration
    p.push(0); // null bitmap (1 byte for 1 param, no NULLs)
    p.push(1); // new_params_bound_flag
    p.push(0x03); // MYSQL_TYPE_LONG
    p.push(0); // signed
    p.extend_from_slice(&value.to_le_bytes());
    p
}

/// Build COM_STMT_EXECUTE with one TEXT param.
fn build_execute_text(stmt_id: u32, value: &str) -> Vec<u8> {
    let mut p = vec![0x17];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    p.push(0);
    p.extend_from_slice(&1u32.to_le_bytes());
    p.push(0); // null bitmap
    p.push(1); // new_params_bound
    p.push(0xfd); // MYSQL_TYPE_VAR_STRING
    p.push(0); // signed
    // lenenc string body: < 251 bytes → single-byte length prefix.
    p.push(value.len() as u8);
    p.extend_from_slice(value.as_bytes());
    p
}

#[test]
fn prepare_select_with_placeholder_returns_columns_and_params() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL, name TEXT)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT id, name FROM t WHERE id = $1");
    let (stmt_id, num_columns, num_params) = read_prepare_ok(&mut s);
    assert!(stmt_id > 0);
    assert_eq!(num_columns, 2);
    assert_eq!(num_params, 1);
}

#[test]
fn execute_dml_with_int_param_inserts_row() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "INSERT INTO t VALUES ($1)");
    let (stmt_id, num_columns, num_params) = read_prepare_ok(&mut s);
    assert_eq!(num_columns, 0);
    assert_eq!(num_params, 1);

    write_packet(&mut s, 0, &build_execute_int(stmt_id, 42));
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
    let (affected, _) = read_lenenc(&ok, 1);
    assert_eq!(affected, 1);

    // Verify via text-protocol SELECT.
    send_query(&mut s, "SELECT id FROM t");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    // v7.39 (round 504) — EOF closing the column definitions.
    let (_seq, cols_eof) = read_packet(&mut s);
    assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
    let (_seq, row) = read_packet(&mut s);
    let (val, _) = read_lenenc_string(&row, 0);
    assert_eq!(val, b"42");
    let _ = read_packet(&mut s); // trailing OK
}

#[test]
fn execute_with_text_param_round_trips() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (s TEXT)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "INSERT INTO t VALUES ($1)");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    write_packet(&mut s, 0, &build_execute_text(stmt_id, "hello world"));
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);

    send_query(&mut s, "SELECT s FROM t");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    // v7.39 (round 504) — EOF closing the column definitions.
    let (_seq, cols_eof) = read_packet(&mut s);
    assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
    let (_seq, row) = read_packet(&mut s);
    let (val, _) = read_lenenc_string(&row, 0);
    assert_eq!(val, b"hello world");
    let _ = read_packet(&mut s);
}

#[test]
fn execute_with_null_param_inserts_null() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "INSERT INTO t VALUES ($1)");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);

    // EXECUTE with NULL via the null bitmap.
    let mut p = vec![0x17];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    p.push(0);
    p.extend_from_slice(&1u32.to_le_bytes());
    p.push(0x01); // null bitmap: bit 0 set
    p.push(1); // new_params_bound
    p.push(0x06); // MYSQL_TYPE_NULL
    p.push(0); // signed
    // No payload — NULL has no encoded body.
    write_packet(&mut s, 0, &p);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);

    send_query(&mut s, "SELECT id FROM t");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    // v7.39 (round 504) — EOF closing the column definitions.
    let (_seq, cols_eof) = read_packet(&mut s);
    assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
    let (_seq, row) = read_packet(&mut s);
    assert_eq!(row[0], 0xfb);
    let _ = read_packet(&mut s);
}

#[test]
fn close_then_execute_returns_unknown_handler() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "INSERT INTO t VALUES ($1)");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);

    // COM_STMT_CLOSE = 0x19 + stmt_id LE
    let mut p = vec![0x19];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    write_packet(&mut s, 0, &p);
    // No reply from CLOSE — go straight to a fresh command.
    write_packet(&mut s, 0, &build_execute_int(stmt_id, 1));
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1243);
}

#[test]
fn reset_replies_with_ok_packet() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "INSERT INTO t VALUES ($1)");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    let mut p = vec![0x1a];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    write_packet(&mut s, 0, &p);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
}

#[test]
fn execute_with_unknown_stmt_id_returns_1243() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    write_packet(&mut s, 0, &build_execute_int(9999, 1));
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1243);
}

#[test]
fn prepare_parse_error_returns_1064() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_prepare(&mut s, "SELEKT 1");
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1064);
}
