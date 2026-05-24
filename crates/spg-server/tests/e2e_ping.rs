//! End-to-end smoke test for the spg-server binary.
//!
//! Spawns the daemon as a child process on an ephemeral port, connects with a
//! raw `TcpStream`, exercises the self-built wire codec, and asserts the reply
//! is `PONG`. This is the v0.1 acceptance gate.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, FrameError, Op, decode, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

fn pick_free_addr() -> String {
    // Bind to port 0 to let the OS choose an unused port, then drop the
    // probe so the daemon can rebind. A racing process could grab the port
    // in the gap — acceptable for v0.1 local smoke tests.
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);
    addr.to_string()
}

fn spawn_server(addr: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn spg-server")
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
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    ping_once(&mut stream);
}

#[test]
fn ten_pings_on_one_connection_all_get_pong() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    for _ in 0..10 {
        ping_once(&mut stream);
    }
}
