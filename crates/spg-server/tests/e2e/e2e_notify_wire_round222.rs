//! v7.39 (round 222) — NOTIFY delivery on the pgwire: an 'A'
//! NotificationResponse ([i32 pid][cstr channel][cstr payload]) reaches
//! the client after the statement completes. Cross-connection: a NOTIFY
//! from one connection reaches a LISTEN from another at that
//! connection's next statement boundary (the engine-shared queue; idle
//! push is a recorded non-goal).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-notify-wire-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
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

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// Run one simple query; collect every 'A' NotificationResponse seen
/// before ReadyForQuery as (channel, payload).
fn exec_collect_notifies(s: &mut TcpStream, sql: &str) -> Vec<(String, String)> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut notifies = Vec::new();
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'A' => {
                // [i32 pid][cstr channel][cstr payload]
                let rest = &body[4..];
                let c_end = rest.iter().position(|&b| b == 0).unwrap();
                let channel = String::from_utf8_lossy(&rest[..c_end]).into_owned();
                let p_rest = &rest[c_end + 1..];
                let p_end = p_rest.iter().position(|&b| b == 0).unwrap();
                let payload = String::from_utf8_lossy(&p_rest[..p_end]).into_owned();
                notifies.push((channel, payload));
            }
            b'E' => panic!("unexpected error for {sql:?}"),
            b'Z' => return notifies,
            _ => {}
        }
    }
}

#[test]
fn notify_reaches_listener_on_the_wire() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut a = pg_connect(addr);

    assert_eq!(exec_collect_notifies(&mut a, "LISTEN chan"), vec![]);
    // Same-connection: the notification arrives with the NOTIFY's own
    // response cycle.
    let got = exec_collect_notifies(&mut a, "NOTIFY chan, 'hi'");
    assert_eq!(got, vec![("chan".to_string(), "hi".to_string())]);

    // Cross-connection: B notifies, A receives at its next statement.
    let mut b = pg_connect(addr);
    exec_collect_notifies(&mut b, "NOTIFY chan, 'from-b'");
    let got = exec_collect_notifies(&mut a, "SELECT 1");
    assert_eq!(got, vec![("chan".to_string(), "from-b".to_string())]);
}
