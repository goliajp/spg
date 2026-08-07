//! v7.39 (round 504) — result-set framing follows the capabilities the
//! CLIENT took, not the ones the server advertised.
//!
//! A real `mariadb:11` client could not read a single row out of SPG. Any
//! row-returning SELECT — `SELECT 1` included — died client-side with
//! `ERROR 2000 (HY000) Unknown or undefined error code`, while DDL and DML
//! went through and even returned proper MySQL error codes. Nothing was
//! logged, because from the server's side nothing had gone wrong.
//!
//! The handshake response was parsed into `client_capabilities` and then
//! dropped, so every encoder framed against `SERVER_CAPABILITIES` instead —
//! and that advertises CLIENT_DEPRECATE_EOF, which MariaDB's client library
//! does not implement. SPG therefore left out the EOF that closes the
//! column definitions for every client alive, and its own e2e suite pinned
//! that, because the suite asserted SPG's output rather than a measurement.
//!
//! Both expectations below are hexdumps off MariaDB 11 (`spg-bench-mariadb`)
//! answering `SELECT 1`, once per branch:
//!
//!   without CLIENT_DEPRECATE_EOF     with CLIENT_DEPRECATE_EOF
//!     01                               01                        column count
//!     03 64 65 66 …                    03 64 65 66 …             column def
//!     fe 00 00 02 00                   —                         (closes defs)
//!     01 31                            01 31                     row
//!     fe 00 00 02 00                   fe 00 00 02 00 00 00      terminator
//!
//! Note the second branch's terminator: an OK packet whose header is 0xfe,
//! not 0x00. The header is load-bearing — in that position a 0x00-headed
//! packet is a valid ROW whose first column is the empty string.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::common;

const READ_TIMEOUT: Duration = Duration::from_secs(20);
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;

fn unique_tmpdir(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "spg-e2e-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).expect("read header");
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("read payload");
    (hdr[3], payload)
}

fn write_packet(stream: &mut TcpStream, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    stream
        .write_all(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, seqno])
        .unwrap();
    stream.write_all(payload).unwrap();
}

/// Walk the handshake taking exactly `caps`, leaving the stream in the
/// command phase.
fn auth_with(addr: &str, caps: u32) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, _greeting) = read_packet(&mut s);
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"anyone\0");
    payload.push(0); // empty auth response → open mode accepts
    payload.extend_from_slice(b"mysql_native_password\0");
    write_packet(&mut s, 1, &payload);
    let (_seq, ok) = read_packet(&mut s);
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
    let dir = unique_tmpdir("deprecate-eof-504");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

/// Collect the packets `SELECT 1` produces, stopping at the first packet
/// that could be a terminator so the SHAPE is what gets asserted.
fn select_one_packets(s: &mut TcpStream) -> Vec<Vec<u8>> {
    send_query(s, "SELECT 1");
    let mut out = Vec::new();
    loop {
        let (_seq, pkt) = read_packet(s);
        let terminal = pkt[0] == 0xfe && pkt.len() < 9;
        out.push(pkt);
        // Two EOFs without the capability, one terminator with it; either
        // way the LAST packet of the set is a short 0xfe-headed one.
        if terminal && out.len() > 2 {
            let eofs = out.iter().filter(|p| p[0] == 0xfe && p.len() < 9).count();
            if eofs == 2 || out.len() == 4 {
                return out;
            }
        }
        assert!(out.len() < 12, "runaway result set: {out:?}");
    }
}

/// A client that does NOT take the capability — a `mariadb` CLI, and every
/// harness in this suite — must get both EOF markers.
#[test]
fn round504_a_client_without_deprecate_eof_gets_both_eof_markers() {
    let (_guard, addr) = spawn();
    let mut s = auth_with(
        &addr,
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH,
    );
    let pkts = select_one_packets(&mut s);
    assert_eq!(
        pkts.len(),
        5,
        "count / def / EOF / row / EOF — got {pkts:?}"
    );
    assert_eq!(pkts[0], vec![0x01], "one column");
    assert_eq!(
        pkts[2],
        vec![0xfe, 0x00, 0x00, 0x02, 0x00],
        "closes the defs"
    );
    assert_eq!(pkts[3], vec![0x01, b'1'], "the row");
    assert_eq!(
        pkts[4],
        vec![0xfe, 0x00, 0x00, 0x02, 0x00],
        "closes the rows"
    );
}

/// A client that DOES take it gets no intermediate marker, and a terminator
/// that is an OK packet under a 0xfe header.
#[test]
fn round504_a_client_with_deprecate_eof_gets_the_modern_framing() {
    let (_guard, addr) = spawn();
    let mut s = auth_with(
        &addr,
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_DEPRECATE_EOF,
    );
    let pkts = select_one_packets(&mut s);
    assert_eq!(pkts.len(), 4, "count / def / row / OK — got {pkts:?}");
    assert_eq!(pkts[0], vec![0x01], "one column");
    assert_eq!(
        pkts[2],
        vec![0x01, b'1'],
        "the row, with no marker before it"
    );
    assert_eq!(
        pkts[3],
        vec![0xfe, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00],
        "OK-under-0xfe terminator"
    );
}

/// The two branches must agree on everything except the framing: same
/// column count, same column definition, same row bytes.
#[test]
fn round504_the_two_branches_carry_identical_data() {
    let (_guard, addr) = spawn();
    let mut plain = auth_with(
        &addr,
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH,
    );
    let mut modern = auth_with(
        &addr,
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_DEPRECATE_EOF,
    );
    let a = select_one_packets(&mut plain);
    let b = select_one_packets(&mut modern);
    assert_eq!(a[0], b[0], "column count");
    assert_eq!(a[1], b[1], "column definition");
    assert_eq!(a[3], b[2], "row");
}

/// The same rule governs COM_FIELD_LIST, whose column definitions end in a
/// marker of their own.
#[test]
fn round504_com_field_list_terminator_follows_the_same_rule() {
    let (_guard, addr) = spawn();
    for (caps, want_header, label) in [
        (
            CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH,
            0xfe_u8,
            "without the capability",
        ),
        (
            CLIENT_PROTOCOL_41
                | CLIENT_SECURE_CONNECTION
                | CLIENT_PLUGIN_AUTH
                | CLIENT_DEPRECATE_EOF,
            0xfe_u8,
            "with the capability",
        ),
    ] {
        let mut s = auth_with(&addr, caps);
        send_query(&mut s, "CREATE TABLE IF NOT EXISTS fl504 (id INT NOT NULL)");
        let (_seq, ok) = read_packet(&mut s);
        assert_eq!(ok[0], 0x00, "DDL OK ({label})");

        let mut payload = vec![0x04]; // COM_FIELD_LIST
        payload.extend_from_slice(b"fl504\0");
        write_packet(&mut s, 0, &payload);
        let last = loop {
            let (_seq, pkt) = read_packet(&mut s);
            if pkt[0] == want_header && pkt.len() < 9 {
                break pkt;
            }
            assert_eq!(pkt[0], 0x03, "a column def starts with lenenc \"def\"");
        };
        assert_eq!(last[0], want_header, "terminator header ({label})");
    }
}
