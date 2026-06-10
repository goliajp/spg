//! Auth model: optional single password via `SPG_PASSWORD`. Mirrors the
//! Valkey/Redis surface — no password means open, a configured password
//! means every non-`Ping` frame is gated behind a successful `AUTH`.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{
    Frame, FrameError, Op, build_auth, build_query, decode, encode, parse_error_response,
};

fn local_spawn(password: Option<&str>) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new();
    if let Some(pw) = password {
        b = b.env("SPG_PASSWORD", pw).keep_env("SPG_PASSWORD");
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(2);

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match decode(&buf) {
            Ok((frame, _)) => return frame,
            Err(FrameError::ShortHeader | FrameError::ShortPayload) => {
                let n = stream.read(&mut chunk).expect("read");
                assert!(n > 0, "server closed connection mid-frame");
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => panic!("decode error: {e}"),
        }
    }
}

fn send(stream: &mut TcpStream, frame: &Frame) {
    let mut out = Vec::new();
    encode(frame, &mut out).expect("encode");
    stream.write_all(&out).expect("write");
}

#[test]
fn query_without_auth_is_rejected_when_password_is_set() {
    let (raw, addrs) = local_spawn(Some("hunter2"));
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_query("SELECT 1"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(msg.contains("authentication required"), "got {msg:?}");
}

#[test]
fn ping_always_allowed_even_without_auth() {
    let (raw, addrs) = local_spawn(Some("hunter2"));
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &Frame::ping());
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::Pong);
}

#[test]
fn wrong_password_keeps_connection_unauthenticated() {
    let (raw, addrs) = local_spawn(Some("hunter2"));
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_auth("nope"));
    let reject = read_frame(&mut s);
    assert_eq!(reject.op, Op::ErrorResponse);

    // A subsequent Query must still be gated.
    send(&mut s, &build_query("SELECT 1"));
    let denied = read_frame(&mut s);
    assert_eq!(denied.op, Op::ErrorResponse);
}

#[test]
fn correct_password_unlocks_queries() {
    let (raw, addrs) = local_spawn(Some("hunter2"));
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_auth("hunter2"));
    let ok = read_frame(&mut s);
    assert_eq!(ok.op, Op::Pong);

    send(&mut s, &build_query("SELECT 1"));
    let rd = read_frame(&mut s);
    assert_eq!(rd.op, Op::RowDescription, "expected RowDescription");
}

#[test]
fn open_server_accepts_auth_no_op() {
    // Open instances (no SPG_PASSWORD set) should still accept AUTH
    // frames gracefully so clients with auth wired in keep working
    // against unauthenticated deployments.
    let (raw, addrs) = local_spawn(None);
    let mut child = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_auth("anything"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::Pong);
}
