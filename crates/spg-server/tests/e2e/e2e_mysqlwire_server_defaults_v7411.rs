//! v7.40.11 — the mysql wire got neither the server's `-c` settings nor
//! the recorded `ALTER DATABASE/ROLE SET` defaults.
//!
//! Two gaps of one shape, found by auditing the first half of this
//! release rather than by a report:
//!
//! ```text
//!                                     pgwire session   mysql-wire session
//!   ALTER ROLE … SET (v7.39 r547)     applied          never
//!   server -c name=value (v7.40.11)   applied          never
//! ```
//!
//! So an operator who set something for the whole deployment got it on
//! one wire and not the other, and a `ALTER ROLE … SET` recorded for a
//! user was honoured only if that user connected over PostgreSQL. Both
//! now go through one function on the engine, in PostgreSQL's order of
//! specificity, so the two hosts cannot drift again.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-mywire-defaults-{label}-{nanos}"));
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
    assert_eq!(ok[0], 0x00, "handshake refused");
    s
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

fn is_eof(pkt: &[u8]) -> bool {
    pkt.first() == Some(&0xfe) && pkt.len() < 9
}

/// The first field of the first row, or `None` for an empty result.
fn scalar(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let (_seq, cc) = read_packet(s);
    assert_ne!(cc.first(), Some(&0xff), "{sql}: server returned an error");
    let (col_count, _) = read_lenenc(&cc, 0);
    for _ in 0..col_count {
        let _ = read_packet(s);
    }
    // A pre-4.1 EOF after the column definitions, when the connection
    // did not negotiate DEPRECATE_EOF.
    let (_seq, maybe_eof) = read_packet(s);
    let mut first = if is_eof(&maybe_eof) {
        read_packet(s).1
    } else {
        maybe_eof
    };
    let mut out = None;
    loop {
        if is_eof(&first) {
            return out;
        }
        if out.is_none() {
            let (v, _) = read_lenenc_string(&first, 0);
            out = Some(String::from_utf8(v).unwrap());
        }
        first = read_packet(s).1;
    }
}

/// The server's own `-c name=value`, on the wire that never saw one.
#[test]
fn a_boot_setting_reaches_a_mysql_wire_session() {
    let dir = unique_dir("boot");
    let (child, addrs) = common::ServerBuilder::new()
        .arg("-c")
        .arg("TimeZone=Asia/Tokyo")
        .arg_path(&dir.join("d.spgdb"))
        .with_mysqlwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.mysqlwire.expect("mysql-wire addr");

    let mut s = auth_open(&addr);
    assert_eq!(
        scalar(&mut s, "SELECT @@time_zone").as_deref(),
        Some("Asia/Tokyo"),
        "the deployment's setting must reach this wire too"
    );
    // Every connection, not just the first.
    let mut s2 = auth_open(&addr);
    assert_eq!(
        scalar(&mut s2, "SELECT @@time_zone").as_deref(),
        Some("Asia/Tokyo")
    );
}

/// And the recorded `ALTER ROLE … SET`, which pgwire has applied since
/// v7.39 round 547 and this host never did.
///
/// `ALTER ROLE ALL SET` is PostgreSQL's every-role scope, which is the
/// one `apply_db_role_settings` reads first and the one that needs no
/// role to exist — creating one here turns on authentication for that
/// name and the harness's handshake carries no password.
#[test]
fn an_alter_role_setting_reaches_a_mysql_wire_session() {
    let dir = unique_dir("role");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_mysqlwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.mysqlwire.expect("mysql-wire addr");

    // Record it through the same wire, then open a NEW connection: the
    // defaults are applied when a session is installed.
    let mut setter = auth_open(&addr);
    let sql = "ALTER ROLE ALL SET TimeZone = 'Asia/Tokyo'";
    send_query(&mut setter, sql);
    let (_seq, resp) = read_packet(&mut setter);
    assert_ne!(
        resp.first(),
        Some(&0xff),
        "{sql}: {}",
        String::from_utf8_lossy(&resp[1..resp.len().min(120)])
    );

    let mut fresh = auth_open(&addr);
    assert_eq!(
        scalar(&mut fresh, "SELECT @@time_zone").as_deref(),
        Some("Asia/Tokyo"),
        "a role's recorded default must reach a mysql-wire session"
    );
}

/// The control: with no `-c` and no recorded default, the wire answers
/// the boot default — so the two tests above are reading a setting that
/// was applied, not one that was already there.
#[test]
fn without_either_the_wire_answers_the_boot_default() {
    let dir = unique_dir("plain");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .with_mysqlwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    let mut s = auth_open(&addr);
    let tz = scalar(&mut s, "SELECT @@time_zone");
    assert_ne!(
        tz.as_deref(),
        Some("Asia/Tokyo"),
        "the fixture's own value must not be the default, or it proves nothing"
    );
    assert!(tz.is_some(), "the variable still answers: {tz:?}");
}
