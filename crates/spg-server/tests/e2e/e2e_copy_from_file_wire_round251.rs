//! v7.39 (round 251) — `COPY t FROM '<file>'` over the pgwire: the
//! SERVER process reads the file, exactly PG's semantics (probed live
//! 2026-07-19). Before this round the statement fell through to the
//! engine, whose refusal leaked the internal `copy_from_buffer` host
//! contract over the wire as 42000.
//!
//! Aligned live: superuser file COPY works (`COPY 3`); a missing file
//! is 58P01 `could not open file "…" for reading: No such file or
//! directory` with the psql \copy HINT; a non-privileged role is 42501
//! `permission denied to COPY from a file` (SPG's admin is the
//! superuser / pg_read_server_files analog). The rows ride the same
//! BEGIN / per-row INSERT+WAL / COMMIT-or-ROLLBACK sequence as the
//! round-250 STDIN path — atomic, and durable across kill -9.

use crate::common;
use spg_crypto::{base64, hmac, pbkdf2, sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-copyfile-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.extend_from_slice(b"\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_typed(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

/// Open a pgwire connection with no auth configured (bench user).
fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "bench");
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// Full SCRAM-SHA-256 login (the same protocol modern PG drivers run).
fn scram_login(addr: &str, user: &str, password: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    let (ty, body) = pg_msg(&mut s);
    assert_eq!(ty, b'R', "expected auth request");
    let subtype = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    assert_eq!(subtype, 10, "expected AuthenticationSASL");
    let client_nonce = "r251-client-nonce";
    let client_first_bare = format!("n={user},r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");
    let mut p1 = Vec::new();
    p1.extend_from_slice(b"SCRAM-SHA-256\0");
    p1.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    p1.extend_from_slice(client_first.as_bytes());
    send_typed(&mut s, b'p', &p1);
    let (ty, body) = pg_msg(&mut s);
    assert_eq!(ty, b'R');
    assert_eq!(u32::from_be_bytes([body[0], body[1], body[2], body[3]]), 11);
    let server_first = std::str::from_utf8(&body[4..]).unwrap().to_string();
    let (mut combined_nonce, mut salt_b64, mut iters) = (String::new(), String::new(), 0u32);
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
    let client_final_without_proof = format!("c=biws,r={combined_nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let salted = pbkdf2::pbkdf2_sha256_32(password.as_bytes(), &salt, iters);
    let client_key = hmac::hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256::hash(&client_key);
    let client_signature = hmac::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut client_proof = [0u8; 32];
    for (i, p) in client_proof.iter_mut().enumerate() {
        *p = client_key[i] ^ client_signature[i];
    }
    let client_final = format!(
        "{client_final_without_proof},p={}",
        base64::encode(&client_proof)
    );
    send_typed(&mut s, b'p', client_final.as_bytes());
    loop {
        let (ty, _body) = pg_msg(&mut s);
        if ty == b'Z' {
            break;
        }
        assert_ne!(ty, b'E', "SCRAM login failed for {user}");
    }
    s
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_typed(s, b'Q', &body);
}

/// Run to ReadyForQuery; returns (CommandComplete tag, first error).
fn exec(s: &mut TcpStream, sql: &str) -> (Option<String>, Option<(String, String)>) {
    send_query(s, sql);
    let mut tag = None;
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'C' => {
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                tag = Some(String::from_utf8_lossy(&body[..end]).into_owned());
            }
            b'E' => {
                let (mut code, mut msg) = (String::new(), String::new());
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let t = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
                    let val = String::from_utf8_lossy(&body[pos..end]).into_owned();
                    match t {
                        b'C' => code = val,
                        b'M' => msg = val,
                        _ => {}
                    }
                    pos = end + 1;
                }
                err = Some((code, msg));
            }
            b'Z' => return (tag, err),
            _ => {}
        }
    }
}

fn first_cell(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let mut cell = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                cell = Some(String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned());
            }
            b'Z' => return cell.expect("no DataRow"),
            _ => {}
        }
    }
}

#[test]
fn file_copy_lands_with_options_and_is_atomic() {
    let dir = unique_dir("happy");
    let good = dir.join("in.csv");
    std::fs::write(&good, "id,name,v\n1,a,10\n2,\"b,comma\",\n3,c,30\n").unwrap();
    let bad = dir.join("bad.csv");
    std::fs::write(&bad, "9,z,90\n10,y,notanint\n").unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec(&mut s, "CREATE TABLE ct (id int, name text, v int)").1, None);
    // Happy path: engine option grammar (FORMAT csv, HEADER) applies.
    let (tag, err) = exec(
        &mut s,
        &format!("COPY ct FROM '{}' WITH (FORMAT csv, HEADER)", good.display()),
    );
    assert_eq!(err, None);
    assert_eq!(tag.as_deref(), Some("COPY 3"));
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "3");
    // A bad row aborts the WHOLE file COPY — none of its rows land.
    let (_, err) = exec(
        &mut s,
        &format!("COPY ct FROM '{}' WITH (FORMAT csv)", bad.display()),
    );
    let (code, msg) = err.expect("bad row must fail");
    assert_eq!(code, "22P02", "{msg}");
    assert!(msg.contains("invalid input syntax for type integer: \"notanint\""), "{msg}");
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM ct"), "3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_takes_pgs_58p01_and_leaks_no_internals() {
    let dir = unique_dir("nofile");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    assert_eq!(exec(&mut s, "CREATE TABLE ct (id int)").1, None);
    let (_, err) = exec(&mut s, "COPY ct FROM '/definitely/not/here.txt'");
    let (code, msg) = err.expect("missing file must fail");
    assert_eq!(code, "58P01", "{msg}");
    assert!(
        msg.contains("could not open file \"/definitely/not/here.txt\" for reading:"),
        "{msg}"
    );
    // The engine-internal host contract must not leak any more.
    assert!(!msg.contains("copy_from_buffer"), "{msg}");
    // Relation check runs BEFORE the file open (PG's order).
    let (_, err) = exec(&mut s, "COPY nope FROM '/also/not/here.txt'");
    let (code, msg) = err.expect("missing relation must fail");
    assert_eq!(code, "42P01", "{msg}");
    assert!(msg.contains("relation \"nope\" does not exist"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_copy_survives_kill9_on_a_wal_server() {
    let dir = unique_dir("wal");
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    let csv = dir.join("in.csv");
    std::fs::write(&csv, "1,a\n2,b\n3,c\n").unwrap();
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let mut guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        assert_eq!(exec(&mut s, "CREATE TABLE ct (id int, name text)").1, None);
        let (tag, err) = exec(
            &mut s,
            &format!("COPY ct FROM '{}' WITH (FORMAT csv)", csv.display()),
        );
        assert_eq!(err, None);
        assert_eq!(tag.as_deref(), Some("COPY 3"));
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    {
        let (raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
            .spawn();
        let _guard = common::ChildGuard(raw);
        let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
        assert_eq!(
            first_cell(&mut s, "SELECT count(*) FROM ct"),
            "3",
            "file-COPY'd rows lost across kill -9"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_admin_role_gets_pgs_42501() {
    let dir = unique_dir("role");
    let csv = dir.join("in.csv");
    std::fs::write(&csv, "1\n").unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_pgwire()
        .env("SPG_ADMIN_PASSWORD", "admin-pw")
        .spawn();
    let _guard = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    {
        let mut s = scram_login(addr, "admin", "admin-pw");
        assert_eq!(exec(&mut s, "CREATE TABLE ct (id int)").1, None);
        assert_eq!(
            exec(&mut s, "CREATE USER 'bi' WITH PASSWORD 'bi-pw' ROLE 'readwrite'").1,
            None
        );
    }
    let mut s = scram_login(addr, "bi", "bi-pw");
    // readwrite CAN insert…
    assert_eq!(exec(&mut s, "INSERT INTO ct VALUES (7)").1, None);
    // …and CAN COPY FROM STDIN (PG: anyone) — but NOT from a file.
    let (_, err) = exec(&mut s, &format!("COPY ct FROM '{}'", csv.display()));
    let (code, msg) = err.expect("non-admin file COPY must fail");
    assert_eq!(code, "42501", "{msg}");
    assert!(msg.contains("permission denied to COPY from a file"), "{msg}");
    // Admin can.
    let mut s = scram_login(addr, "admin", "admin-pw");
    let (tag, err) = exec(&mut s, &format!("COPY ct FROM '{}'", csv.display()));
    assert_eq!(err, None);
    assert_eq!(tag.as_deref(), Some("COPY 1"));
    let _ = std::fs::remove_dir_all(&dir);
}
