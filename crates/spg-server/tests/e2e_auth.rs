//! Auth model: optional single password via `SPG_PASSWORD`. Mirrors the
//! Valkey/Redis surface — no password means open, a configured password
//! means every non-`Ping` frame is gated behind a successful `AUTH`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{
    Frame, FrameError, Op, build_auth, build_query, decode, encode, parse_error_response,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);
    addr.to_string()
}

fn spawn_server_with_password(addr: &str, password: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(pw) = password {
        cmd.env("SPG_PASSWORD", pw);
    } else {
        cmd.env_remove("SPG_PASSWORD");
    }
    cmd.spawn().expect("spawn spg-server")
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early with {status:?} (last connect err: {e})");
                }
                assert!(
                    Instant::now() < deadline,
                    "server never accepted connections after {STARTUP_TIMEOUT:?}: {e}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

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
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_password(&addr, Some("hunter2")));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_query("SELECT 1"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(msg.contains("authentication required"), "got {msg:?}");
}

#[test]
fn ping_always_allowed_even_without_auth() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_password(&addr, Some("hunter2")));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &Frame::ping());
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::Pong);
}

#[test]
fn wrong_password_keeps_connection_unauthenticated() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_password(&addr, Some("hunter2")));
    let mut s = wait_for_listener(&addr, &mut child.0);
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
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_password(&addr, Some("hunter2")));
    let mut s = wait_for_listener(&addr, &mut child.0);
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
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_password(&addr, None));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_auth("anything"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::Pong);
}
