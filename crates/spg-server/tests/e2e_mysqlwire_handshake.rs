//! v7.17.0 Phase 3.P0-70 — MySQL wire-protocol foundation.
//!
//! End-to-end: spawn an spg-server child with `SPG_MYSQLWIRE_ADDR`
//! set, raw-TCP-connect, read the HandshakeV10 greeting, parse
//! every field against the spec, build a HandshakeResponse41,
//! send it, and verify the server replies with the ERR packet
//! that points to the auth surface landing in P0-71.
//!
//! No real MySQL client involved — these tests run with just the
//! std::net TCP shape so the verification stays on the bytes.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let p = std::env::temp_dir().join(format!("spg-e2e-mysqlwire-{pid}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_with_mysqlwire(db: &std::path::Path) -> (common::ChildGuard, String) {
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(db)
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr published");
    (common::ChildGuard(child), addr)
}

/// Read one MySQL packet — header (3-byte len + 1-byte seqno)
/// then payload. Mirrors mysqlwire::read_packet but lives in the
/// test crate (no cross-crate access to the internal helper).
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
    let hdr = [
        len as u8,
        (len >> 8) as u8,
        (len >> 16) as u8,
        seqno,
    ];
    stream.write_all(&hdr).expect("write header");
    stream.write_all(payload).expect("write payload");
}

#[test]
fn handshake_v10_greeting_carries_every_required_field() {
    let dir = unique_tmpdir();
    let db: PathBuf = dir.join("spg.db");
    let (_guard, addr) = spawn_with_mysqlwire(&db);

    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let (seqno, payload) = read_packet(&mut s);
    assert_eq!(seqno, 0, "server's first packet is always seqno=0");
    // protocol_version = 10
    assert_eq!(payload[0], 10);
    // server_version is a NUL-terminated string.
    let nul = payload[1..].iter().position(|b| *b == 0).expect("server_version NUL");
    let server_version =
        std::str::from_utf8(&payload[1..1 + nul]).expect("server_version utf8");
    assert!(
        server_version.starts_with("8.0.0-spg"),
        "expected 8.0-lineage advertisement, got {server_version}"
    );
    let pos = 1 + nul + 1;
    // connection_id u32 LE
    let connection_id = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
    assert_ne!(connection_id, 0);
    let pos = pos + 4;
    // auth_plugin_data_part_1 — 8 bytes scramble
    let scramble_part1 = &payload[pos..pos + 8];
    assert_eq!(scramble_part1.len(), 8);
    let pos = pos + 8;
    // 1 byte filler 0x00
    assert_eq!(payload[pos], 0);
    let pos = pos + 1;
    // 2 bytes lower cap flags
    let cap_lo = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap()) as u32;
    let pos = pos + 2;
    // 1 byte charset (utf8mb4 = 0xff)
    assert_eq!(payload[pos], 0xff);
    let pos = pos + 1;
    // 2 bytes server status flags
    let _status = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
    let pos = pos + 2;
    // 2 bytes upper cap flags
    let cap_hi = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap()) as u32;
    let caps = cap_lo | (cap_hi << 16);
    // CLIENT_PROTOCOL_41 must be set on the server side or no
    // modern client will negotiate.
    assert!(caps & 0x0000_0200 != 0, "CLIENT_PROTOCOL_41 must be set");
    assert!(caps & 0x0008_0000 != 0, "CLIENT_PLUGIN_AUTH must be set");
    let pos = pos + 2;
    // 1 byte auth_plugin_data_len (we advertise 21)
    assert_eq!(payload[pos], 21);
    let pos = pos + 1;
    // 10 reserved zeros
    assert_eq!(&payload[pos..pos + 10], &[0u8; 10]);
    let pos = pos + 10;
    // auth_plugin_data_part_2 — 12 bytes scramble + NUL.
    let scramble_part2 = &payload[pos..pos + 12];
    assert_eq!(scramble_part2.len(), 12);
    let pos = pos + 12;
    assert_eq!(payload[pos], 0);
    let pos = pos + 1;
    // null-terminated auth plugin name
    let rest = &payload[pos..];
    let nul = rest.iter().position(|b| *b == 0).expect("auth plugin name NUL");
    let plugin = std::str::from_utf8(&rest[..nul]).unwrap();
    assert_eq!(plugin, "mysql_native_password");

    // Scramble totals 20 bytes; printable ASCII only — matches
    // what real MySQL servers send.
    let scramble: Vec<u8> = scramble_part1
        .iter()
        .chain(scramble_part2.iter())
        .copied()
        .collect();
    assert_eq!(scramble.len(), 20);
    for b in &scramble {
        assert!((0x21..=0x7e).contains(b));
    }
}

#[test]
fn server_replies_to_handshake_response_with_ok_in_open_mode() {
    // P0-70 originally asserted an ERR pointing at the
    // not-yet-implemented auth surface; P0-71 wires auth and
    // the open-mode (no users) path now flips to OK. The deeper
    // auth-table flow is exercised in e2e_mysqlwire_auth.
    let dir = unique_tmpdir();
    let db: PathBuf = dir.join("spg.db");
    let (_guard, addr) = spawn_with_mysqlwire(&db);

    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _payload) = read_packet(&mut s);

    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"root\0");
    payload.push(0); // empty auth_response — open mode accepts it
    payload.extend_from_slice(b"mysql_native_password\0");
    write_packet(&mut s, 1, &payload);

    let (seqno, payload) = read_packet(&mut s);
    assert_eq!(seqno, 2, "server's reply to client seqno=1 is seqno=2");
    assert_eq!(payload[0], 0x00, "expected OK packet in open mode");
}

#[test]
fn server_returns_err_when_client_seqno_is_wrong() {
    let dir = unique_tmpdir();
    let db: PathBuf = dir.join("spg.db");
    let (_guard, addr) = spawn_with_mysqlwire(&db);

    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _payload) = read_packet(&mut s);

    // Client sends seqno=3 (intentionally wrong — should be 1).
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"root\0");
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    write_packet(&mut s, 3, &payload);

    let (_seqno, payload) = read_packet(&mut s);
    assert_eq!(payload[0], 0xff, "expected ERR packet");
    let msg = std::str::from_utf8(&payload[9..]).unwrap();
    assert!(
        msg.contains("seqno"),
        "expected seqno-mismatch message, got {msg:?}"
    );
}
