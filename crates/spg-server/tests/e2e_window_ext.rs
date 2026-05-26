#![allow(clippy::doc_markdown, clippy::float_cmp, clippy::uninlined_format_args)]

//! v4.21 extended window functions — LAG / LEAD / FIRST_VALUE /
//! LAST_VALUE / NTH_VALUE / NTILE / PERCENT_RANK / CUME_DIST.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

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
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

fn select_rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut out = Vec::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => out.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => out.extend(parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return out,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn as_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn as_i64_opt(v: &WireValue) -> Option<i64> {
    match v {
        WireValue::Null => None,
        _ => Some(as_i64(v)),
    }
}

fn as_f64(v: &WireValue) -> f64 {
    match v {
        WireValue::Float(f) => *f,
        WireValue::Int(n) => f64::from(*n),
        #[allow(clippy::cast_precision_loss)]
        WireValue::BigInt(n) => *n as f64,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected numeric, got {other:?}"),
    }
}

fn seed_ts(s: &mut TcpStream) {
    exec_ok(s, "CREATE TABLE ts (n INT NOT NULL, v INT NOT NULL)");
    for (n, v) in [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
        exec_ok(s, &format!("INSERT INTO ts VALUES ({n}, {v})"));
    }
}

#[test]
fn lag_default_offset_and_default_value() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    let rows = select_rows(&mut s, "SELECT n, LAG(v) OVER (ORDER BY n) FROM ts");
    let mut got: Vec<(i64, Option<i64>)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64_opt(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    assert_eq!(
        got,
        vec![
            (1, None),
            (2, Some(10)),
            (3, Some(20)),
            (4, Some(30)),
            (5, Some(40))
        ]
    );
}

#[test]
fn lead_with_offset_and_default() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    let rows = select_rows(&mut s, "SELECT n, LEAD(v, 2, -1) OVER (ORDER BY n) FROM ts");
    let mut got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    // lead +2: 1→v(3)=30; 2→v(4)=40; 3→v(5)=50; 4→default -1; 5→default -1.
    assert_eq!(got, vec![(1, 30), (2, 40), (3, 50), (4, -1), (5, -1)]);
}

#[test]
fn first_and_last_value_honor_frame() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    // FIRST_VALUE over default ordered frame: stays at v(1)=10.
    // LAST_VALUE over default ordered frame (UNBOUNDED PRECEDING
    // AND CURRENT ROW, peer-aware): equals current row's value.
    let rows = select_rows(
        &mut s,
        "SELECT n, FIRST_VALUE(v) OVER (ORDER BY n), LAST_VALUE(v) OVER (ORDER BY n) FROM ts",
    );
    let mut got: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64(&r[1]), as_i64(&r[2])))
        .collect();
    got.sort_by_key(|(n, _, _)| *n);
    assert_eq!(
        got,
        vec![
            (1, 10, 10),
            (2, 10, 20),
            (3, 10, 30),
            (4, 10, 40),
            (5, 10, 50)
        ]
    );
}

#[test]
fn nth_value_within_frame() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    // Whole-partition frame (UNBOUNDED BOTH): nth_value(v, 2) = 20 everywhere.
    let rows = select_rows(
        &mut s,
        "SELECT n, NTH_VALUE(v, 2) OVER (ORDER BY n ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM ts",
    );
    for r in &rows {
        assert_eq!(as_i64(&r[1]), 20);
    }
}

#[test]
fn ntile_distributes_evenly_and_handles_remainders() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    // 5 rows / 2 buckets → first bucket gets 3, second gets 2.
    let rows = select_rows(&mut s, "SELECT n, NTILE(2) OVER (ORDER BY n) FROM ts");
    let mut got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_i64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    assert_eq!(got, vec![(1, 1), (2, 1), (3, 1), (4, 2), (5, 2)]);
}

#[test]
fn percent_rank_spans_zero_to_one() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    seed_ts(&mut s);

    let rows = select_rows(&mut s, "SELECT n, PERCENT_RANK() OVER (ORDER BY n) FROM ts");
    let mut got: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_f64(&r[1])))
        .collect();
    got.sort_by_key(|(n, _)| *n);
    // (rank-1)/(n-1) for ranks 1..5 in n=5: 0, 0.25, 0.5, 0.75, 1.0.
    assert_eq!(
        got,
        vec![(1, 0.0), (2, 0.25), (3, 0.5), (4, 0.75), (5, 1.0)]
    );
}

#[test]
fn cume_dist_is_peer_inclusive_running_share() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE peers (k INT NOT NULL)");
    for k in [1, 2, 2, 3] {
        exec_ok(&mut s, &format!("INSERT INTO peers VALUES ({k})"));
    }

    let rows = select_rows(&mut s, "SELECT k, CUME_DIST() OVER (ORDER BY k) FROM peers");
    let mut got: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (as_i64(&r[0]), as_f64(&r[1])))
        .collect();
    got.sort_by_key(|(k, _)| *k);
    // k=1: 1/4=0.25; k=2 peers both 3/4=0.75; k=3: 4/4=1.0.
    assert_eq!(got[0], (1, 0.25));
    assert_eq!(got[1], (2, 0.75));
    assert_eq!(got[2], (2, 0.75));
    assert_eq!(got[3], (3, 1.0));
}
