//! v7.17.0 Phase 3.P0-75 — admin commands: PING / INIT_DB /
//! FIELD_LIST.

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
        crate::common::tmp_base().join(format!("spg-e2e-mysqlwire-admin-{label}-{pid}-{nanos}"));
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
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03);
    payload.extend_from_slice(sql.as_bytes());
    write_packet(s, 0, &payload);
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

#[test]
fn com_ping_returns_ok_packet() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    // COM_PING = 0x0e
    write_packet(&mut s, 0, &[0x0e]);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
}

#[test]
fn com_init_db_with_any_nonempty_name_returns_ok() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    // COM_INIT_DB = 0x02 + db name (no terminating NUL)
    let mut payload = vec![0x02];
    payload.extend_from_slice(b"mydatabase");
    write_packet(&mut s, 0, &payload);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "expected OK for valid USE");
}

#[test]
fn com_init_db_empty_name_returns_err() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    write_packet(&mut s, 0, &[0x02]);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1049, "ER_BAD_DB_ERROR");
}

#[test]
fn com_field_list_returns_column_defs_for_existing_table() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(
        &mut s,
        "CREATE TABLE users (id INT NOT NULL, name TEXT, age INT)",
    );
    let (_seq, _ok) = read_packet(&mut s);

    // COM_FIELD_LIST = 0x04 + table\0
    let mut payload = vec![0x04];
    payload.extend_from_slice(b"users\0");
    write_packet(&mut s, 0, &payload);

    let mut col_names = Vec::new();
    loop {
        let (_seq, pkt) = read_packet(&mut s);
        // v7.39 (round 504) — this harness does not take
        // CLIENT_DEPRECATE_EOF, so COM_FIELD_LIST ends in an EOF packet.
        // It used to end in an OK because SPG framed against the
        // capabilities it advertised rather than the ones taken.
        if pkt[0] == 0xfe && pkt.len() < 9 {
            break;
        }
        // Parse column_def_41 — 5 lenenc strings prefix, name in 5th.
        let mut pos = 0;
        for _ in 0..4 {
            let (_s, c) = read_lenenc_string(&pkt, pos);
            pos += c;
        }
        let (name, _) = read_lenenc_string(&pkt, pos);
        col_names.push(String::from_utf8(name).unwrap());
    }
    assert_eq!(col_names, vec!["id", "name", "age"]);
}

#[test]
fn com_field_list_unknown_table_returns_1146() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    let mut payload = vec![0x04];
    payload.extend_from_slice(b"nope\0");
    write_packet(&mut s, 0, &payload);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1146);
    assert_eq!(&err[4..9], b"42S02");
}

#[test]
fn multiple_commands_keep_sequence_aligned() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    // PING then a query then PING — each new command resets seqno
    // so the wire bytes stay consistent.
    write_packet(&mut s, 0, &[0x0e]);
    let (_seq, ok1) = read_packet(&mut s);
    assert_eq!(ok1[0], 0x00);

    send_query(&mut s, "SELECT 7 AS lucky");
    let (_seq, _cc) = read_packet(&mut s);
    let (_seq, _col) = read_packet(&mut s);
    let (_seq, cols_eof) = read_packet(&mut s);
    assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
    let (_seq, row) = read_packet(&mut s);
    let (val, _) = read_lenenc_string(&row, 0);
    assert_eq!(val, b"7");
    let (_seq, trailing) = read_packet(&mut s);
    assert_eq!(trailing[0], 0xfe, "trailing EOF");

    write_packet(&mut s, 0, &[0x0e]);
    let (_seq, ok2) = read_packet(&mut s);
    assert_eq!(ok2[0], 0x00);
}
