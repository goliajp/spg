//! v4.2 resource limits end-to-end:
//! - `SPG_MAX_CONNECTIONS` rejects the (N+1)-th client with a clear
//!   error and lets existing clients keep working.
//! - `SPG_MAX_QUERY_ROWS` makes a SELECT that would exceed the cap
//!   surface `query exceeded max_query_rows=N` instead of streaming
//!   millions of rows.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_error_response};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn spawn_server(addr: &str, envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.env_remove("SPG_PASSWORD");
    cmd.env_remove("SPG_ADMIN_PASSWORD");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
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
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut buf = Vec::new();
    encode(f, &mut buf).unwrap();
    s.write_all(&buf).unwrap();
}

#[test]
fn max_connections_rejects_overflow_with_clear_error() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &[("SPG_MAX_CONNECTIONS", "2")]));
    let _ = wait_for_listener(&addr, &mut child.0);

    // Two long-lived clients claim the slots.
    let s1 = TcpStream::connect(&addr).unwrap();
    let s2 = TcpStream::connect(&addr).unwrap();
    s1.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // The third client gets accepted at the TCP layer but the
    // server should immediately send an error frame and close.
    // Allow a small window for the server thread to notice + reply
    // before we read.
    thread::sleep(Duration::from_millis(50));
    let mut s3 = TcpStream::connect(&addr).unwrap();
    s3.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let f = read_frame(&mut s3);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.contains("max_connections"),
        "expected max_connections hint, got {msg:?}"
    );

    // Existing clients still work after overflow.
    drop(s3);
    let mut s1 = s1; // shadow to allow send
    send(&mut s1, &build_query("CREATE TABLE t (id INT NOT NULL)"));
    assert_eq!(read_frame(&mut s1).op, Op::CommandComplete);

    // Once one slot frees, a fresh client can connect again.
    drop(s2);
    thread::sleep(Duration::from_millis(50));
    let mut s4 = TcpStream::connect(&addr).unwrap();
    s4.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send(&mut s4, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s4);
    assert_eq!(
        f.op,
        Op::RowDescription,
        "freed slot should accept a working client"
    );
}

#[test]
fn max_query_rows_caps_select_result() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &[("SPG_MAX_QUERY_ROWS", "3")]));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send(&mut s, &build_query("CREATE TABLE t (id INT NOT NULL)"));
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    for i in 1..=5 {
        send(&mut s, &build_query(&format!("INSERT INTO t VALUES ({i})")));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    }

    // 5 rows exceeds the cap of 3 — should refuse without streaming.
    send(&mut s, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.contains("max_query_rows=3"),
        "expected row-cap hint, got {msg:?}"
    );

    // Sub-cap query still works.
    send(&mut s, &build_query("SELECT * FROM t LIMIT 3"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::RowDescription, "LIMIT 3 should fit under cap");
}
