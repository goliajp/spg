//! End-to-end SPG: spawn the daemon binary, drive it with raw wire frames,
//! verify the full chain CREATE → INSERT → SELECT works. Lives in the
//! `spg-server` crate because that's the only one whose integration tests
//! have `CARGO_BIN_EXE_spg-server` injected by cargo.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{
    Frame, Op, WireType, WireValue, build_query, encode, parse_command_complete, parse_data_row,
    parse_error_response, parse_row_description,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
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
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

/// Read exactly one frame off the wire. Uses `read_exact` so we never drop
/// already-arrived bytes between consecutive frames.
fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read frame header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read frame payload");
    }
    Frame { op, payload }
}

#[test]
fn create_insert_select_full_cycle() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut stream,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );
    assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);

    for sql in [
        "INSERT INTO t VALUES (1, 'alice')",
        "INSERT INTO t VALUES (2, 'bob')",
    ] {
        send_query(&mut stream, sql);
        let f = read_frame(&mut stream);
        assert_eq!(f.op, Op::CommandComplete);
        assert_eq!(parse_command_complete(&f).unwrap(), 1);
    }

    send_query(&mut stream, "SELECT * FROM t");
    let rd = read_frame(&mut stream);
    assert_eq!(rd.op, Op::RowDescription);
    let cols = parse_row_description(&rd).unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].ty, WireType::Int);
    assert_eq!(cols[1].name, "name");
    assert_eq!(cols[1].ty, WireType::Text);

    let mut rows = Vec::new();
    loop {
        let f = read_frame(&mut stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], WireValue::Int(1));
    assert_eq!(rows[0][1], WireValue::Text("alice".into()));
    assert_eq!(rows[1][0], WireValue::Int(2));
    assert_eq!(rows[1][1], WireValue::Text("bob".into()));
}

#[test]
fn select_with_where_via_wire() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut stream, "CREATE TABLE nums (x INT NOT NULL)");
    assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);
    for sql in [
        "INSERT INTO nums VALUES (1)",
        "INSERT INTO nums VALUES (2)",
        "INSERT INTO nums VALUES (3)",
    ] {
        send_query(&mut stream, sql);
        assert_eq!(read_frame(&mut stream).op, Op::CommandComplete);
    }
    send_query(&mut stream, "SELECT * FROM nums WHERE x > 1");
    assert_eq!(read_frame(&mut stream).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(&mut stream);
        match f.op {
            Op::DataRow => count += 1,
            Op::CommandComplete => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(count, 2);
}

#[test]
fn syntax_error_returns_error_response() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut stream = wait_for_listener(&addr, &mut child.0);
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(&mut stream, "DROP TABLE foo"); // not in parser scope yet
    let f = read_frame(&mut stream);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.to_ascii_lowercase().contains("parse") || msg.contains("expected"),
        "error message should mention parsing — got {msg:?}"
    );
}

#[test]
fn second_connection_sees_first_connection_writes() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut a = wait_for_listener(&addr, &mut child.0);
    a.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut a, "CREATE TABLE shared (v INT)");
    assert_eq!(read_frame(&mut a).op, Op::CommandComplete);
    send_query(&mut a, "INSERT INTO shared VALUES (42)");
    assert_eq!(read_frame(&mut a).op, Op::CommandComplete);

    let mut b = TcpStream::connect(&addr).unwrap();
    b.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_query(&mut b, "SELECT * FROM shared");
    assert_eq!(read_frame(&mut b).op, Op::RowDescription);
    let dr = read_frame(&mut b);
    assert_eq!(dr.op, Op::DataRow);
    let values = parse_data_row(&dr).unwrap();
    assert_eq!(values[0], WireValue::Int(42));
    assert_eq!(read_frame(&mut b).op, Op::CommandComplete);
}
