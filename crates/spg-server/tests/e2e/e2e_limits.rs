//! v4.2 resource limits end-to-end:
//! - `SPG_MAX_CONNECTIONS` rejects the (N+1)-th client with a clear
//!   error and lets existing clients keep working.
//! - `SPG_MAX_QUERY_ROWS` makes a SELECT that would exceed the cap
//!   surface `query exceeded max_query_rows=N` instead of streaming
//!   millions of rows.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_error_response};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Spawn an spg-server on an OS-chosen port with extra env vars. Returns
/// `(child, addr)` from stderr's "listening on" line. See `e2e_wal_binary`
/// for the race-avoidance rationale vs the old probe-and-drop pattern.
fn spawn_server(envs: &[(&str, &str)]) -> (Child, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg("127.0.0.1:0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.env_remove("SPG_PASSWORD");
    cmd.env_remove("SPG_ADMIN_PASSWORD");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let stderr = child.stderr.take().expect("piped stderr");
    let addr = read_listening_addr(&mut child, stderr);
    (child, addr)
}

fn read_listening_addr(child: &mut Child, stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited before printing listen addr: {status:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(_) => {
                if let Some(addr) = extract_addr(&line) {
                    thread::spawn(move || {
                        let mut sink = String::new();
                        let _ = reader.read_to_string(&mut sink);
                    });
                    return addr;
                }
            }
            Err(e) => panic!("read stderr: {e}"),
        }
    }
    let _ = child.kill();
    panic!("server didn't print listen addr within {STARTUP_TIMEOUT:?}");
}

fn extract_addr(line: &str) -> Option<String> {
    let after = line.find("listening on ")?;
    let tail = &line[after + "listening on ".len()..];
    let end = tail.find([' ', '\n', '\r']).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {addr}: {e}");
                thread::sleep(Duration::from_millis(10));
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
    let (raw_child, addr) = spawn_server(&[("SPG_MAX_CONNECTIONS", "2")]);
    let _child = ChildGuard(raw_child);

    // Two long-lived clients claim the slots.
    let s1 = TcpStream::connect(&addr).unwrap();
    let s2 = TcpStream::connect(&addr).unwrap();
    s1.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Before the overflow probe, prove the server has REGISTERED both
    // slot-holders, not merely that the kernel accepted their sockets.
    // v7.37 (round 827) — a round-trip on each is that proof; the flat
    // 50ms this replaces raced the accept loop under load, and losing
    // the race hands s3 a free slot and fails the overflow assertion.
    let mut s1 = s1;
    let mut s2 = s2;
    let round_trip = |s: &mut TcpStream| {
        send(s, &build_query("SELECT 1"));
        loop {
            let f = read_frame(s);
            if matches!(f.op, Op::CommandComplete | Op::ErrorResponse | Op::Error) {
                break;
            }
        }
    };
    round_trip(&mut s1);
    round_trip(&mut s2);

    // The third client gets accepted at the TCP layer but the
    // server should immediately send an error frame and close.
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
    send(&mut s1, &build_query("CREATE TABLE t (id INT NOT NULL)"));
    assert_eq!(read_frame(&mut s1).op, Op::CommandComplete);

    // Once one slot frees, a fresh client can connect again. The
    // server noticing s2's close is not observable from outside, so
    // v7.37 (round 827) retries the whole probe to a deadline instead
    // of betting 50ms on it; the assertion on the final reply is
    // unchanged.
    drop(s2);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let f = loop {
        let mut s4 = TcpStream::connect(&addr).unwrap();
        s4.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send(&mut s4, &build_query("SELECT * FROM t"));
        let f = read_frame(&mut s4);
        if f.op != Op::ErrorResponse || std::time::Instant::now() >= deadline {
            break f;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        f.op,
        Op::RowDescription,
        "freed slot should accept a working client"
    );
}

#[test]
fn max_query_rows_caps_select_result() {
    let (raw_child, addr) = spawn_server(&[("SPG_MAX_QUERY_ROWS", "3")]);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
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
