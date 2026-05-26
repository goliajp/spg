#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.9 JSON column type — round-trip through the native wire.
//! No path operators yet; the column just stores and returns the
//! verbatim JSON string.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn spawn_server(addr: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .spawn()
        .unwrap()
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
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    let f = read_frame(s);
    assert_eq!(
        f.op,
        Op::CommandComplete,
        "expected CC for {sql:?}, got {:?}",
        f.op
    );
}

#[test]
fn json_column_round_trips_arbitrary_payload() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE doc (id INT NOT NULL, body JSON NOT NULL)",
    );
    let payload = r#"{"name":"alice","scores":[1,2,3],"nested":{"k":true}}"#;
    exec_ok(&mut s, &format!("INSERT INTO doc VALUES (1, '{payload}')"));

    send(&mut s, &build_query("SELECT body FROM doc"));
    let rd = read_frame(&mut s);
    assert_eq!(rd.op, Op::RowDescription);
    let dr = read_frame(&mut s);
    assert_eq!(dr.op, Op::DataRow);
    let cells = parse_data_row(&dr).unwrap();
    assert_eq!(cells.len(), 1);
    match &cells[0] {
        WireValue::Text(s) => assert_eq!(s, payload),
        other => panic!("expected Text wire value for JSON, got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}

#[test]
fn jsonb_alias_accepted() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // JSONB should map to the same JSON column type; PG drivers use
    // it interchangeably.
    exec_ok(&mut s, "CREATE TABLE d (b JSONB NOT NULL)");
    exec_ok(&mut s, "INSERT INTO d VALUES ('{\"a\":1}')");
    send(&mut s, &build_query("SELECT b FROM d"));
    let _rd = read_frame(&mut s);
    let dr = read_frame(&mut s);
    let cells = parse_data_row(&dr).unwrap();
    match &cells[0] {
        WireValue::Text(s) => assert!(s.contains("\"a\":1")),
        other => panic!("got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}

#[test]
fn update_json_column() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE doc (id INT NOT NULL, body JSON NOT NULL)",
    );
    exec_ok(&mut s, "INSERT INTO doc VALUES (1, '{\"v\":1}')");
    exec_ok(&mut s, "UPDATE doc SET body = '{\"v\":2}' WHERE id = 1");

    send(&mut s, &build_query("SELECT body FROM doc"));
    let _rd = read_frame(&mut s);
    let dr = read_frame(&mut s);
    let cells = parse_data_row(&dr).unwrap();
    match &cells[0] {
        WireValue::Text(s) => assert_eq!(s, "{\"v\":2}"),
        other => panic!("got {other:?}"),
    }
    let _cc = read_frame(&mut s);
}
