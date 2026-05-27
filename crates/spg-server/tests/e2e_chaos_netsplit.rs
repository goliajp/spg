#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::similar_names,
    clippy::uninlined_format_args
)]

//! v4.36 replication chaos: a tiny TCP proxy sits between primary
//! and follower, the test flips it into "disconnect" mode mid-stream,
//! lets the master keep writing, then restores the proxy. After
//! reconnect the follower must catch up to exactly the master's row
//! count — no duplicates, no gaps.
//!
//! Also exercises the v4.36 status-frame protocol extension via
//! the follower's `/metrics` endpoint: `spg_replication_lag_*`
//! series must appear and update once the master sends status
//! frames.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(10);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-netsplit-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn(addr: &str, db: &PathBuf, wal: &PathBuf, extra_env: &[(&str, String)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in extra_env {
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
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn wait_for_addr(addr: &str) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "addr never came up: {addr}");
        thread::sleep(Duration::from_millis(50));
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
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected SQL {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut count: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => count = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                count = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return count,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}

// ---- the proxy ----

/// Tiny stdlib-only TCP proxy. Each accepted connection spawns two
/// forwarder threads (client→backend and backend→client). When the
/// shared `connected` flag flips to `false`, both directions tear
/// down their sockets so the follower observes a clean EOF and
/// reconnects via the existing `run_follower` retry loop. Flipping
/// back to `true` re-opens the gate; the proxy's accept loop keeps
/// running across the cycle.
#[derive(Clone)]
struct ProxyControl {
    connected: Arc<AtomicBool>,
}

impl ProxyControl {
    fn new() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(true)),
        }
    }
    fn netsplit(&self) {
        self.connected.store(false, Ordering::Release);
    }
    fn heal(&self) {
        self.connected.store(true, Ordering::Release);
    }
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
}

fn spawn_proxy(listen_addr: &str, backend_addr: String) -> ProxyControl {
    let listener = TcpListener::bind(listen_addr).expect("proxy bind");
    listener.set_nonblocking(true).unwrap();
    let ctrl = ProxyControl::new();
    let ctrl_for_thread = ctrl.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            // Drain the non-blocking accept buffer; sleep when empty.
            let client = match stream {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(_) => return,
            };
            if !ctrl_for_thread.is_connected() {
                let _ = client.shutdown(Shutdown::Both);
                continue;
            }
            let Ok(backend) = TcpStream::connect(&backend_addr) else {
                let _ = client.shutdown(Shutdown::Both);
                continue;
            };
            let _ = client.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = backend.set_read_timeout(Some(Duration::from_millis(200)));
            let c1 = client.try_clone().unwrap();
            let b1 = backend.try_clone().unwrap();
            let ctrl_a = ctrl_for_thread.clone();
            let ctrl_b = ctrl_for_thread.clone();
            thread::spawn(move || pump(c1, backend, &ctrl_a));
            thread::spawn(move || pump(b1, client, &ctrl_b));
        }
    });
    ctrl
}

fn pump(mut src: TcpStream, mut dst: TcpStream, ctrl: &ProxyControl) {
    let mut buf = [0u8; 4096];
    loop {
        if !ctrl.is_connected() {
            let _ = src.shutdown(Shutdown::Both);
            let _ = dst.shutdown(Shutdown::Both);
            return;
        }
        match src.read(&mut buf) {
            Ok(0) => {
                let _ = dst.shutdown(Shutdown::Both);
                return;
            }
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() {
                    return;
                }
                let _ = dst.flush();
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Polling tick — re-check the kill switch. Loop
                // header handles continuation; no explicit continue
                // needed.
            }
            Err(_) => return,
        }
    }
}

// ---- follower-catch-up helper ----

fn wait_for_count(addr: &str, sql: &str, expected: i64, deadline: Instant) -> i64 {
    loop {
        if let Ok(mut s) = TcpStream::connect(addr) {
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let actual = select_int(&mut s, sql);
            if actual == expected {
                return actual;
            }
            if Instant::now() >= deadline {
                return actual;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn http_get(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect /metrics");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

// ---- the tests ----

/// Disconnect the follower mid-stream, keep writing on the master,
/// then heal the proxy. Final row counts must match exactly — no
/// duplicates, no gaps. This is PROD_READY row 2.9.
#[test]
fn netsplit_disconnect_then_heal_resyncs_without_loss_or_dup() {
    let dir_p = unique_tmpdir("pri");
    let dir_f = unique_tmpdir("fol");
    let primary_native = pick_free_addr();
    let primary_repl = pick_free_addr();
    let proxy_addr = pick_free_addr();
    let follower_native = pick_free_addr();

    let mut primary = ChildGuard(spawn(
        &primary_native,
        &dir_p.join("a.db"),
        &dir_p.join("a.wal"),
        &[("SPG_REPL_ADDR", primary_repl.clone())],
    ));
    let mut prim_client = wait_for_listener(&primary_native, &mut primary.0);
    prim_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // Wait for the replication listener too.
    wait_for_addr(&primary_repl);

    let proxy_ctrl = spawn_proxy(&proxy_addr, primary_repl.clone());
    // The proxy needs a moment to bind before the follower connects.
    wait_for_addr(&proxy_addr);

    let mut follower = ChildGuard(spawn(
        &follower_native,
        &dir_f.join("a.db"),
        &dir_f.join("a.wal"),
        &[("SPG_FOLLOW_OF", proxy_addr.clone())],
    ));
    let _ = wait_for_listener(&follower_native, &mut follower.0);

    exec_ok(
        &mut prim_client,
        "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)",
    );
    for i in 0..10 {
        exec_ok(
            &mut prim_client,
            &format!("INSERT INTO t VALUES ({i}, {})", i * 7),
        );
    }
    // Initial catch-up.
    let pre_break = wait_for_count(
        &follower_native,
        "SELECT count(*) FROM t",
        10,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(
        pre_break, 10,
        "follower must catch up to 10 rows before split"
    );

    // Netsplit — close all active proxy sockets and reject new ones.
    proxy_ctrl.netsplit();
    // Master keeps writing while the follower is cut off.
    for i in 10..25 {
        exec_ok(
            &mut prim_client,
            &format!("INSERT INTO t VALUES ({i}, {})", i * 7),
        );
    }
    // Confirm the follower is in fact behind (catch-up timeout
    // should not have fired since the proxy is down).
    {
        let mut s = TcpStream::connect(&follower_native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let stuck = select_int(&mut s, "SELECT count(*) FROM t");
        assert!(
            stuck < 25,
            "follower should be behind during netsplit; saw {stuck} rows"
        );
    }

    // Heal — the follower's reconnect loop kicks in via run_follower's
    // RECONNECT_DELAY (500 ms) and replays from the offset it last
    // applied. No duplicates because the master sends only [pos..].
    proxy_ctrl.heal();
    let post_heal = wait_for_count(
        &follower_native,
        "SELECT count(*) FROM t",
        25,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(
        post_heal, 25,
        "follower must converge to exactly 25 rows after heal — no dup, no gap"
    );
    // Sanity: the row values themselves are intact too (covers the
    // "duplicates" case where count happens to land at 25 but with
    // wrong values).
    let mut s = TcpStream::connect(&follower_native).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let sum = select_int(&mut s, "SELECT sum(v) FROM t");
    let expected_sum: i64 = (0..25).map(|i| (i * 7) as i64).sum();
    assert_eq!(
        sum, expected_sum,
        "row contents must reflect exactly the 25 INSERTs"
    );
}

/// v4.36 status-frame protocol gives the follower visibility into
/// the master's WAL position. `/metrics` on the follower exposes
/// both `spg_replication_lag_bytes` and `spg_replication_lag_seconds`
/// once it has applied at least one status frame.
#[test]
fn follower_metrics_expose_replication_lag_after_status_frame() {
    let dir_p = unique_tmpdir("lp");
    let dir_f = unique_tmpdir("lf");
    let primary_native = pick_free_addr();
    let primary_repl = pick_free_addr();
    let follower_native = pick_free_addr();
    let follower_http = pick_free_addr();

    let mut primary = ChildGuard(spawn(
        &primary_native,
        &dir_p.join("a.db"),
        &dir_p.join("a.wal"),
        &[("SPG_REPL_ADDR", primary_repl.clone())],
    ));
    let mut prim_client = wait_for_listener(&primary_native, &mut primary.0);
    prim_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(&primary_repl);

    let mut follower = ChildGuard(spawn(
        &follower_native,
        &dir_f.join("a.db"),
        &dir_f.join("a.wal"),
        &[
            ("SPG_FOLLOW_OF", primary_repl.clone()),
            ("SPG_HTTP_ADDR", follower_http.clone()),
        ],
    ));
    let _ = wait_for_listener(&follower_native, &mut follower.0);
    wait_for_addr(&follower_http);

    exec_ok(&mut prim_client, "CREATE TABLE lag (id INT NOT NULL)");
    for i in 0..5 {
        exec_ok(&mut prim_client, &format!("INSERT INTO lag VALUES ({i})"));
    }
    let _ = wait_for_count(
        &follower_native,
        "SELECT count(*) FROM lag",
        5,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    // Give the status frame timer (50 ms cadence on the master) at
    // least a few iterations to land.
    thread::sleep(Duration::from_millis(300));

    let metrics = http_get(&follower_http, "/metrics");
    assert!(
        metrics.contains("spg_replication_lag_bytes"),
        "/metrics missing lag_bytes series; got:\n{metrics}"
    );
    assert!(
        metrics.contains("spg_replication_lag_seconds"),
        "/metrics missing lag_seconds series; got:\n{metrics}"
    );
}
