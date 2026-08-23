//! v7.17.0 Phase 3.P0-71 — `mysql_native_password` auth verification
//! end-to-end against the live spg-server.
//!
//! Five angles:
//!   1. Open mode (no users registered) → any client passes,
//!      server replies with OK.
//!   2. Users-present mode + correct password → OK.
//!   3. Users-present mode + wrong password → ERR(28000 access denied).
//!   4. Users-present mode + unknown user → ERR(28000).
//!   5. Caching_sha2 plugin name → ERR(08004) pointing at P0-72.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-mysqlwire-auth-{label}-{pid}-{nanos}"));
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

/// Extract the 20-byte scramble from a HandshakeV10 greeting.
fn extract_scramble(payload: &[u8]) -> Vec<u8> {
    assert_eq!(payload[0], 10);
    let nul = payload[1..].iter().position(|b| *b == 0).unwrap();
    let pos = 1 + nul + 1 + 4; // server_version + NUL + conn_id
    let part1 = &payload[pos..pos + 8];
    // pos + 8 + 1 (filler) + 2 (cap_lo) + 1 (charset) + 2 (status) + 2 (cap_hi) + 1 (alpd len) + 10 (reserved)
    let pos2 = pos + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
    let part2 = &payload[pos2..pos2 + 12];
    let mut s = Vec::with_capacity(20);
    s.extend_from_slice(part1);
    s.extend_from_slice(part2);
    s
}

/// `SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))`.
fn native_password_response(scramble: &[u8], password: &str) -> Vec<u8> {
    use sha1::Digest;
    let sha1_pwd = sha1::Sha1::digest(password.as_bytes());
    let sha1_sha1_pwd = sha1::Sha1::digest(sha1_pwd);
    let mut buf = Vec::with_capacity(20 + 20);
    buf.extend_from_slice(scramble);
    buf.extend_from_slice(&sha1_sha1_pwd);
    let mask = sha1::Sha1::digest(&buf);
    sha1_pwd
        .iter()
        .zip(mask.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

fn build_handshake_response(username: &str, auth_response: &[u8], plugin: &str) -> Vec<u8> {
    let caps: u32 = 0x0000_0200 // CLIENT_PROTOCOL_41
        | 0x0000_8000 // CLIENT_SECURE_CONNECTION
        | 0x0008_0000; // CLIENT_PLUGIN_AUTH
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.push(auth_response.len() as u8);
    payload.extend_from_slice(auth_response);
    payload.extend_from_slice(plugin.as_bytes());
    payload.push(0);
    payload
}

fn run_handshake(addr: &str, username: &str, password: Option<&str>) -> (u8, Vec<u8>) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, greeting) = read_packet(&mut s);
    let scramble = extract_scramble(&greeting);
    let auth_response = match password {
        Some(pw) => native_password_response(&scramble, pw),
        None => Vec::new(),
    };
    let resp = build_handshake_response(username, &auth_response, "mysql_native_password");
    write_packet(&mut s, 1, &resp);
    read_packet(&mut s)
}

fn spawn_with_admin(
    label: &str,
    admin_user: &str,
    admin_password: &str,
) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_mysqlwire()
        .keep_env("SPG_ADMIN_PASSWORD")
        .env("SPG_ADMIN_USER", admin_user)
        .env("SPG_ADMIN_PASSWORD", admin_password)
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

fn spawn_open_mode(label: &str) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

#[test]
fn open_mode_accepts_any_password() {
    let (_guard, addr) = spawn_open_mode("open");
    let (seqno, payload) = run_handshake(&addr, "anyone", Some("anything"));
    assert_eq!(seqno, 2);
    assert_eq!(payload[0], 0x00, "expected OK, got {:#x}", payload[0]);
}

#[test]
fn users_present_correct_password_yields_ok() {
    let (_guard, addr) = spawn_with_admin("ok", "alice", "wonderland");
    let (_seqno, payload) = run_handshake(&addr, "alice", Some("wonderland"));
    assert_eq!(payload[0], 0x00, "expected OK, got {:#x}", payload[0]);
}

#[test]
fn users_present_wrong_password_yields_access_denied() {
    let (_guard, addr) = spawn_with_admin("deny", "bob", "swordfish");
    let (_seqno, payload) = run_handshake(&addr, "bob", Some("nope"));
    assert_eq!(payload[0], 0xff, "expected ERR, got {:#x}", payload[0]);
    let errno = u16::from_le_bytes(payload[1..3].try_into().unwrap());
    assert_eq!(errno, 1045, "expected ER_ACCESS_DENIED_ERROR");
    assert_eq!(&payload[4..9], b"28000");
}

#[test]
fn unknown_user_yields_access_denied() {
    let (_guard, addr) = spawn_with_admin("unknown", "carol", "pw");
    let (_seqno, payload) = run_handshake(&addr, "ghost", Some("pw"));
    assert_eq!(payload[0], 0xff);
    let errno = u16::from_le_bytes(payload[1..3].try_into().unwrap());
    assert_eq!(errno, 1045);
}

#[test]
fn unknown_plugin_name_yields_plugin_mismatch() {
    // P0-72 now accepts caching_sha2_password; any plugin we
    // still don't speak (caching_sha2 RSA fallback, sha256, ...)
    // surfaces as the 1251 plugin-mismatch error.
    let (_guard, addr) = spawn_open_mode("plugin");
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _greeting) = read_packet(&mut s);
    let resp = build_handshake_response("user", &[0u8; 20], "no_such_plugin");
    write_packet(&mut s, 1, &resp);
    let (_seqno, payload) = read_packet(&mut s);
    assert_eq!(payload[0], 0xff);
    let errno = u16::from_le_bytes(payload[1..3].try_into().unwrap());
    assert_eq!(errno, 1251);
}
