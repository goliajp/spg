#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]

//! v4.8 SCRAM-SHA-256 e2e — hand-rolled client running the same
//! protocol modern PG drivers run by default.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_crypto::{base64, hmac, pbkdf2, sha256};

mod common;

fn local_spawn(db: &std::path::Path, admin_pw: &str) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .env("SPG_ADMIN_PASSWORD", admin_pw)
        .spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-scram-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

#[test]
fn full_scram_handshake_with_admin_user() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let (raw, addrs) = local_spawn(&db, "hunter2");
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(addrs.pgwire.as_ref().unwrap());
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Startup → server should advertise SCRAM-SHA-256 (the bootstrap
    // admin has scram secrets since it was created by v4.8 engine).
    send_startup(&mut s, "admin");
    let auth = read_message(&mut s);
    assert_eq!(auth.ty, b'R');
    let subtype = u32::from_be_bytes([auth.body[0], auth.body[1], auth.body[2], auth.body[3]]);
    assert_eq!(subtype, 10, "expected AuthenticationSASL (subtype 10)");
    let mech_bytes = &auth.body[4..];
    let mech_str = std::str::from_utf8(mech_bytes).unwrap();
    assert!(
        mech_str.contains("SCRAM-SHA-256"),
        "expected SCRAM-SHA-256 in mech list, got {mech_str:?}"
    );

    // Build client-first
    let client_nonce = "fixed-client-nonce-abcdef";
    let client_first_bare = format!("n=admin,r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");
    // SASLInitialResponse: mech\0 + i32 cf_len + cf bytes
    let mut p1 = Vec::new();
    p1.extend_from_slice(b"SCRAM-SHA-256\0");
    p1.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    p1.extend_from_slice(client_first.as_bytes());
    send_msg(&mut s, b'p', &p1);

    // Server replies SASLContinue with server-first
    let cont = read_message(&mut s);
    assert_eq!(cont.ty, b'R');
    let subtype = u32::from_be_bytes([cont.body[0], cont.body[1], cont.body[2], cont.body[3]]);
    assert_eq!(subtype, 11, "expected SASLContinue (subtype 11)");
    let server_first = std::str::from_utf8(&cont.body[4..]).unwrap().to_string();

    // Parse r= s= i= out of server-first
    let mut combined_nonce = String::new();
    let mut salt_b64 = String::new();
    let mut iters: u32 = 0;
    for attr in server_first.split(',') {
        if let Some(v) = attr.strip_prefix("r=") {
            combined_nonce = v.to_string();
        } else if let Some(v) = attr.strip_prefix("s=") {
            salt_b64 = v.to_string();
        } else if let Some(v) = attr.strip_prefix("i=") {
            iters = v.parse().unwrap();
        }
    }
    assert!(
        combined_nonce.starts_with(client_nonce),
        "combined nonce should start with client nonce"
    );
    let salt = base64::decode(&salt_b64).unwrap();
    assert_eq!(salt.len(), 16);

    // Compute client proof per RFC 5802
    let client_final_without_proof = format!("c=biws,r={combined_nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let salted = pbkdf2::pbkdf2_sha256_32(b"hunter2", &salt, iters);
    let client_key = hmac::hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256::hash(&client_key);
    let client_signature = hmac::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut client_proof = [0u8; 32];
    for i in 0..32 {
        client_proof[i] = client_key[i] ^ client_signature[i];
    }
    let client_final = format!(
        "{client_final_without_proof},p={}",
        base64::encode(&client_proof)
    );
    send_msg(&mut s, b'p', client_final.as_bytes());

    // Server replies SASLFinal (subtype 12) + AuthOk (subtype 0) +
    // ParameterStatus(es) + BackendKeyData + ReadyForQuery.
    let fin = read_message(&mut s);
    assert_eq!(fin.ty, b'R');
    let sub = u32::from_be_bytes([fin.body[0], fin.body[1], fin.body[2], fin.body[3]]);
    assert_eq!(sub, 12, "expected SASLFinal (subtype 12)");
    let sig_str = std::str::from_utf8(&fin.body[4..]).unwrap();
    assert!(sig_str.starts_with("v="), "expected v=... server signature");

    let ok = read_message(&mut s);
    assert_eq!(ok.ty, b'R');
    let sub = u32::from_be_bytes([ok.body[0], ok.body[1], ok.body[2], ok.body[3]]);
    assert_eq!(sub, 0, "expected AuthenticationOk (subtype 0)");

    // Drain to ReadyForQuery to confirm the channel is healthy.
    loop {
        let m = read_message(&mut s);
        if m.ty == b'Z' {
            break;
        }
    }
}

#[test]
fn wrong_password_scram_rejected() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let (raw, addrs) = local_spawn(&db, "hunter2");
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(addrs.pgwire.as_ref().unwrap());
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_startup(&mut s, "admin");
    let _ = read_message(&mut s); // AuthenticationSASL

    let client_nonce = "nonce-aaaaaaaaaaaaaaa";
    let client_first_bare = format!("n=admin,r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");
    let mut p1 = Vec::new();
    p1.extend_from_slice(b"SCRAM-SHA-256\0");
    p1.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    p1.extend_from_slice(client_first.as_bytes());
    send_msg(&mut s, b'p', &p1);

    let cont = read_message(&mut s);
    let server_first = std::str::from_utf8(&cont.body[4..]).unwrap().to_string();
    let mut combined_nonce = String::new();
    let mut salt_b64 = String::new();
    let mut iters: u32 = 0;
    for attr in server_first.split(',') {
        if let Some(v) = attr.strip_prefix("r=") {
            combined_nonce = v.to_string();
        } else if let Some(v) = attr.strip_prefix("s=") {
            salt_b64 = v.to_string();
        } else if let Some(v) = attr.strip_prefix("i=") {
            iters = v.parse().unwrap();
        }
    }
    let salt = base64::decode(&salt_b64).unwrap();

    // Compute proof with WRONG password.
    let client_final_without_proof = format!("c=biws,r={combined_nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let salted = pbkdf2::pbkdf2_sha256_32(b"WRONG", &salt, iters);
    let client_key = hmac::hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256::hash(&client_key);
    let client_signature = hmac::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut client_proof = [0u8; 32];
    for i in 0..32 {
        client_proof[i] = client_key[i] ^ client_signature[i];
    }
    let client_final = format!(
        "{client_final_without_proof},p={}",
        base64::encode(&client_proof)
    );
    send_msg(&mut s, b'p', client_final.as_bytes());

    let err = read_message(&mut s);
    assert_eq!(err.ty, b'E', "expected ErrorResponse on bad proof");
}
