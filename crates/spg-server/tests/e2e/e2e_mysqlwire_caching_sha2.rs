//! v7.17.0 Phase 3.P0-72 — `caching_sha2_password` fast-path
//! verification end-to-end against the live spg-server.
//!
//! Five angles:
//!   1. Open mode + caching_sha2 plugin → Auth More Data
//!      (0x01 0x03 fast auth success) + OK.
//!   2. Users-present + correct password + caching_sha2 → same.
//!   3. Users-present + wrong password + caching_sha2 → ERR
//!      (28000 access denied).
//!   4. Mixed: same user can also auth via mysql_native_password
//!      (verifies both verifiers are populated at create time).
//!   5. Unknown plugin name still returns the plugin-mismatch
//!      error from P0-71.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-mysqlwire-sha2-{label}-{pid}-{nanos}"));
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

fn extract_scramble(payload: &[u8]) -> Vec<u8> {
    assert_eq!(payload[0], 10);
    let nul = payload[1..].iter().position(|b| *b == 0).unwrap();
    let pos = 1 + nul + 1 + 4;
    let part1 = &payload[pos..pos + 8];
    let pos2 = pos + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
    let part2 = &payload[pos2..pos2 + 12];
    let mut s = Vec::with_capacity(20);
    s.extend_from_slice(part1);
    s.extend_from_slice(part2);
    s
}

/// SHA256(password) XOR SHA256(scramble || SHA256(SHA256(password)))
fn caching_sha2_response(scramble: &[u8], password: &str) -> Vec<u8> {
    use sha2::Digest;
    let sha_pwd = sha2::Sha256::digest(password.as_bytes());
    let sha_sha_pwd = sha2::Sha256::digest(sha_pwd);
    let mut buf = Vec::with_capacity(20 + 32);
    buf.extend_from_slice(scramble);
    buf.extend_from_slice(&sha_sha_pwd);
    let mask = sha2::Sha256::digest(&buf);
    sha_pwd
        .iter()
        .zip(mask.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))
fn mysql_native_response(scramble: &[u8], password: &str) -> Vec<u8> {
    use sha1::Digest;
    let sha_pwd = sha1::Sha1::digest(password.as_bytes());
    let sha_sha_pwd = sha1::Sha1::digest(sha_pwd);
    let mut buf = Vec::with_capacity(20 + 20);
    buf.extend_from_slice(scramble);
    buf.extend_from_slice(&sha_sha_pwd);
    let mask = sha1::Sha1::digest(&buf);
    sha_pwd
        .iter()
        .zip(mask.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

fn build_handshake_response(username: &str, auth_response: &[u8], plugin: &str) -> Vec<u8> {
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
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

fn run_handshake(addr: &str, username: &str, plugin: &str, response: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, _greeting) = read_packet(&mut s);
    let payload = build_handshake_response(username, response, plugin);
    write_packet(&mut s, 1, &payload);
    // caching_sha2 success returns two packets (Auth More Data
    // then OK); other paths return one (OK / ERR). Read until
    // we hit OK or ERR.
    let mut out = Vec::new();
    for _ in 0..3 {
        let (seq, p) = read_packet(&mut s);
        let header = p[0];
        out.push((seq, p));
        if header == 0x00 || header == 0xff {
            break;
        }
    }
    out
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

fn spawn_open(label: &str) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

fn scramble_for(addr: &str) -> (TcpStream, Vec<u8>) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seqno, greeting) = read_packet(&mut s);
    let scramble = extract_scramble(&greeting);
    (s, scramble)
}

#[test]
fn caching_sha2_open_mode_returns_fast_auth_success_then_ok() {
    let (_guard, addr) = spawn_open("caching-open");
    let packets = run_handshake(&addr, "anyone", "caching_sha2_password", &[0xab; 32]);
    assert_eq!(packets.len(), 2, "expected AMD + OK packets");
    let (_seq, amd) = &packets[0];
    assert_eq!(amd[0], 0x01, "Auth More Data header");
    assert_eq!(amd[1], 0x03, "fast auth success status");
    let (_seq, ok) = &packets[1];
    assert_eq!(ok[0], 0x00, "OK packet header");
}

#[test]
fn caching_sha2_correct_password_succeeds() {
    let (_guard, addr) = spawn_with_admin("caching-ok", "alice", "wonderland");
    let (mut s, scramble) = scramble_for(&addr);
    let auth = caching_sha2_response(&scramble, "wonderland");
    let resp = build_handshake_response("alice", &auth, "caching_sha2_password");
    write_packet(&mut s, 1, &resp);
    let (_seq, amd) = read_packet(&mut s);
    assert_eq!(amd[0], 0x01);
    assert_eq!(amd[1], 0x03);
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
}

#[test]
fn caching_sha2_wrong_password_fails() {
    let (_guard, addr) = spawn_with_admin("caching-deny", "bob", "swordfish");
    let (mut s, scramble) = scramble_for(&addr);
    let auth = caching_sha2_response(&scramble, "wrong-pw");
    let resp = build_handshake_response("bob", &auth, "caching_sha2_password");
    write_packet(&mut s, 1, &resp);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1045);
    let msg = std::str::from_utf8(&err[9..]).unwrap();
    assert!(
        msg.contains("caching_sha2_password"),
        "expected message to mention plugin, got {msg:?}"
    );
}

#[test]
fn same_user_can_auth_via_either_plugin() {
    // Regression: both verifiers are populated at create time
    // alongside the BLAKE3 / SCRAM hashes. mysql_native still
    // works for legacy clients even after caching_sha2 lands.
    let (_guard, addr) = spawn_with_admin("dual", "carol", "topsecret");
    // First connect: caching_sha2.
    {
        let (mut s, scramble) = scramble_for(&addr);
        let auth = caching_sha2_response(&scramble, "topsecret");
        let resp = build_handshake_response("carol", &auth, "caching_sha2_password");
        write_packet(&mut s, 1, &resp);
        let (_seq, amd) = read_packet(&mut s);
        assert_eq!(amd[0], 0x01);
        let (_seq, ok) = read_packet(&mut s);
        assert_eq!(ok[0], 0x00);
    }
    // Second connect: mysql_native (fresh TCP — server allocates
    // a new scramble per connection).
    {
        let (mut s, scramble) = scramble_for(&addr);
        let auth = mysql_native_response(&scramble, "topsecret");
        let resp = build_handshake_response("carol", &auth, "mysql_native_password");
        write_packet(&mut s, 1, &resp);
        let (_seq, ok) = read_packet(&mut s);
        assert_eq!(
            ok[0], 0x00,
            "mysql_native auth also passes for the same user"
        );
    }
}

#[test]
fn unknown_plugin_still_returns_mismatch_err() {
    let (_guard, addr) = spawn_open("plugin-unknown");
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, _greeting) = read_packet(&mut s);
    let resp = build_handshake_response("user", &[0u8; 20], "totally_made_up");
    write_packet(&mut s, 1, &resp);
    let (_seq, err) = read_packet(&mut s);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes(err[1..3].try_into().unwrap());
    assert_eq!(errno, 1251);
}
