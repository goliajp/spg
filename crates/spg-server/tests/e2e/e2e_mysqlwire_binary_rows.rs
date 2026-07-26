//! v7.17.0 Phase 3.P0-76 — binary result row encoding.
//!
//! COM_STMT_EXECUTE on a SELECT statement now returns rows in
//! the MySQL binary protocol (1-byte 0x00 header + NULL bitmap +
//! per-column binary payload) instead of the text-protocol
//! fallthrough P0-74 shipped. sqlx-mysql and any binary-protocol
//! client decodes typed values directly without re-parsing text.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-mysqlwire-bin-{label}-{pid}-{nanos}"));
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

fn read_prepare_ok(s: &mut TcpStream) -> (u32, u16, u16) {
    let (_seq, payload) = read_packet(s);
    assert_eq!(payload[0], 0x00);
    let stmt_id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
    let num_columns = u16::from_le_bytes(payload[5..7].try_into().unwrap());
    let num_params = u16::from_le_bytes(payload[7..9].try_into().unwrap());
    if num_params > 0 {
        for _ in 0..num_params {
            let _ = read_packet(s);
        }
        let _ = read_packet(s);
    }
    if num_columns > 0 {
        for _ in 0..num_columns {
            let _ = read_packet(s);
        }
        let _ = read_packet(s);
    }
    (stmt_id, num_columns, num_params)
}

/// v7.39 (round 504) — consume the EOF that closes a result set's column
/// definitions. This harness does not take CLIENT_DEPRECATE_EOF, so the
/// server owes it one; SPG used to leave it out for every client, having
/// framed against its advertised capabilities instead of the taken ones.
fn read_columns_eof(s: &mut TcpStream) {
    let (_seq, pkt) = read_packet(s);
    assert_eq!(pkt[0], 0xfe, "EOF closes the column definitions");
    assert_eq!(pkt.len(), 5, "protocol-41 EOF: header + warnings + status");
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

fn execute_no_params(s: &mut TcpStream, stmt_id: u32) {
    let mut p = vec![0x17];
    p.extend_from_slice(&stmt_id.to_le_bytes());
    p.push(0);
    p.extend_from_slice(&1u32.to_le_bytes());
    write_packet(s, 0, &p);
}

/// Helper: parse one binary row out of a packet. Returns the
/// raw byte slices keyed by column ordinal, with `None` for
/// NULL cells.
fn parse_binary_row(payload: &[u8], num_cols: usize) -> Vec<Option<Vec<u8>>> {
    assert_eq!(payload[0], 0x00, "binary row header");
    let bitmap_len = (num_cols + 7 + 2) / 8;
    let bitmap = &payload[1..1 + bitmap_len];
    let mut out: Vec<Option<Vec<u8>>> = Vec::with_capacity(num_cols);
    let mut pos = 1 + bitmap_len;
    for i in 0..num_cols {
        let bit_idx = i + 2;
        let is_null = (bitmap[bit_idx / 8] >> (bit_idx % 8)) & 1 == 1;
        if is_null {
            out.push(None);
            continue;
        }
        // We don't know the column type here — but the test uses
        // known types per-column. Read all the rest as raw bytes
        // and let the test parse them in order.
        out.push(Some(payload[pos..].to_vec()));
        // Advance `pos` past one value based on the consumer's
        // post-processing — see the per-test asserts.
        break;
    }
    out
}

#[test]
fn binary_row_int_column_encodes_as_4_bytes_le() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    let _ = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (42), (1000), (-7)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT id FROM t ORDER BY id");
    let (stmt_id, num_columns, _np) = read_prepare_ok(&mut s);
    assert_eq!(num_columns, 1);
    execute_no_params(&mut s, stmt_id);
    // column_count + 1 column_def + 3 binary rows + trailing OK.
    let _ = read_packet(&mut s); // column_count
    let _ = read_packet(&mut s); // column_def
    read_columns_eof(&mut s);
    let mut got: Vec<i32> = Vec::new();
    for _ in 0..3 {
        let (_seq, pkt) = read_packet(&mut s);
        assert_eq!(pkt[0], 0x00);
        // bitmap_len = (1 + 7 + 2) / 8 = 1
        let bitmap = pkt[1];
        assert_eq!(bitmap & 0b00000100, 0, "column 0 is NOT NULL");
        let v = i32::from_le_bytes(pkt[2..6].try_into().unwrap());
        got.push(v);
    }
    assert_eq!(got, vec![-7, 42, 1000]);
    let (_seq, _ok) = read_packet(&mut s);
}

#[test]
fn binary_row_text_column_uses_lenenc_string() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (name TEXT)");
    let _ = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES ('alpha'), ('bravo')");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT name FROM t ORDER BY name");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    execute_no_params(&mut s, stmt_id);
    let _ = read_packet(&mut s); // column_count
    let _ = read_packet(&mut s); // column_def
    read_columns_eof(&mut s);
    let mut got: Vec<String> = Vec::new();
    for _ in 0..2 {
        let (_seq, pkt) = read_packet(&mut s);
        assert_eq!(pkt[0], 0x00);
        let bitmap = pkt[1];
        assert_eq!(bitmap & 0b00000100, 0);
        let (bytes, _) = read_lenenc_string(&pkt, 2);
        got.push(String::from_utf8(bytes).unwrap());
    }
    assert_eq!(got, vec!["alpha", "bravo"]);
    let _ = read_packet(&mut s);
}

#[test]
fn binary_row_null_column_sets_null_bitmap_bit() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (id INT, marker INT NOT NULL)");
    let _ = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (NULL, 1), (5, 2)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT id FROM t ORDER BY marker");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    execute_no_params(&mut s, stmt_id);
    let _ = read_packet(&mut s); // column_count
    let _ = read_packet(&mut s); // column_def
    read_columns_eof(&mut s);
    let (_seq, pkt1) = read_packet(&mut s);
    assert_eq!(pkt1[0], 0x00);
    let bitmap1 = pkt1[1];
    assert_eq!(bitmap1 & 0b00000100, 0b00000100, "row 1 id is NULL");
    let (_seq, pkt2) = read_packet(&mut s);
    let bitmap2 = pkt2[1];
    assert_eq!(bitmap2 & 0b00000100, 0);
    let v = i32::from_le_bytes(pkt2[2..6].try_into().unwrap());
    assert_eq!(v, 5);
    let _ = read_packet(&mut s);
}

#[test]
fn binary_row_bigint_column_encodes_as_8_bytes_le() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (n BIGINT NOT NULL)");
    let _ = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (9000000000)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT n FROM t");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    execute_no_params(&mut s, stmt_id);
    let _ = read_packet(&mut s); // column_count
    let _ = read_packet(&mut s); // column_def
    read_columns_eof(&mut s);
    let (_seq, pkt) = read_packet(&mut s);
    let v = i64::from_le_bytes(pkt[2..10].try_into().unwrap());
    assert_eq!(v, 9_000_000_000);
    let _ = read_packet(&mut s);
}

#[test]
fn binary_row_multi_column_packs_in_declared_order() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(
        &mut s,
        "CREATE TABLE t (id INT NOT NULL, label TEXT, weight BIGINT)",
    );
    let _ = read_packet(&mut s);
    send_query(
        &mut s,
        "INSERT INTO t VALUES (1, 'first', 100), (2, NULL, 200)",
    );
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT id, label, weight FROM t ORDER BY id");
    let (stmt_id, num_columns, _np) = read_prepare_ok(&mut s);
    assert_eq!(num_columns, 3);
    execute_no_params(&mut s, stmt_id);
    let _ = read_packet(&mut s); // column_count
    for _ in 0..3 {
        let _ = read_packet(&mut s); // column defs
    }
    read_columns_eof(&mut s);
    // bitmap_len = (3 + 7 + 2) / 8 = 1
    let (_seq, row1) = read_packet(&mut s);
    assert_eq!(row1[0], 0x00);
    let bitmap1 = row1[1];
    // All columns non-NULL → no high bits set above bit-2.
    assert_eq!(bitmap1 & 0b00011100, 0);
    let id1 = i32::from_le_bytes(row1[2..6].try_into().unwrap());
    let (label1_bytes, c) = read_lenenc_string(&row1, 6);
    let weight1 = i64::from_le_bytes(row1[6 + c..6 + c + 8].try_into().unwrap());
    assert_eq!(id1, 1);
    assert_eq!(label1_bytes, b"first");
    assert_eq!(weight1, 100);

    let (_seq, row2) = read_packet(&mut s);
    let bitmap2 = row2[1];
    // Column 1 (label) is NULL → bit (1 + 2) = bit 3 set.
    assert_eq!(bitmap2 & 0b00001000, 0b00001000);
    assert_eq!(bitmap2 & 0b00000100, 0); // id present
    assert_eq!(bitmap2 & 0b00010000, 0); // weight present
    let id2 = i32::from_le_bytes(row2[2..6].try_into().unwrap());
    // No label payload (NULL); next 8 bytes are weight.
    let weight2 = i64::from_le_bytes(row2[6..14].try_into().unwrap());
    assert_eq!(id2, 2);
    assert_eq!(weight2, 200);

    let _ = read_packet(&mut s);
}

#[test]
fn binary_row_float_column_encodes_as_8_bytes_le_double() {
    let (_guard, addr) = spawn();
    let mut s = auth_open(&addr);
    send_query(&mut s, "CREATE TABLE t (x FLOAT NOT NULL)");
    let _ = read_packet(&mut s);
    send_query(&mut s, "INSERT INTO t VALUES (3.14159265358979)");
    let _ = read_packet(&mut s);
    send_prepare(&mut s, "SELECT x FROM t");
    let (stmt_id, _nc, _np) = read_prepare_ok(&mut s);
    execute_no_params(&mut s, stmt_id);
    let _ = read_packet(&mut s); // column_count
    let _ = read_packet(&mut s); // column_def
    read_columns_eof(&mut s);
    let (_seq, pkt) = read_packet(&mut s);
    let v = f64::from_le_bytes(pkt[2..10].try_into().unwrap());
    assert!((v - 3.14159265358979).abs() < 1e-15, "got {v}");
    let _ = read_packet(&mut s);
}

// Silence dead_code warning on the dev helper we'll plumb in
// when COM_FIELD_LIST tests grow more elaborate.
#[allow(dead_code)]
fn _anchor(b: &[u8], n: usize) -> Vec<Option<Vec<u8>>> {
    parse_binary_row(b, n)
}
