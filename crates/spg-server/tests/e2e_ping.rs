//! End-to-end smoke test for the spg-server binary.
//!
//! Spawns the daemon as a child process on an ephemeral port, connects with a
//! raw `TcpStream`, exercises the self-built wire codec, and asserts the reply
//! is `PONG`. This is the v0.1 acceptance gate.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{Frame, FrameError, Op, decode, encode};

mod common;
use common::{ChildGuard, ServerBuilder, connect_to};

const READ_TIMEOUT: Duration = Duration::from_secs(2);

fn ping_once(stream: &mut TcpStream) {
    let mut out = Vec::new();
    encode(&Frame::ping(), &mut out).expect("encode ping");
    stream.write_all(&out).expect("write ping");

    let mut buf = Vec::new();
    let mut chunk = [0u8; 32];
    loop {
        match decode(&buf) {
            Ok((frame, _)) => {
                assert_eq!(frame.op, Op::Pong, "expected PONG, got {:?}", frame.op);
                return;
            }
            Err(FrameError::ShortHeader | FrameError::ShortPayload) => {
                let n = stream.read(&mut chunk).expect("read");
                assert!(n > 0, "server closed connection before sending pong");
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => panic!("decode error: {e}"),
        }
    }
}

#[test]
fn ping_pong_round_trip_against_real_daemon() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut stream = connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    ping_once(&mut stream);
}

#[test]
fn ten_pings_on_one_connection_all_get_pong() {
    let (raw, addrs) = ServerBuilder::new().spawn();
    let _child = ChildGuard(raw);
    let mut stream = connect_to(&addrs.native);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    for _ in 0..10 {
        ping_once(&mut stream);
    }
}
