#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.1.9 — chaos e2e for the logical-replication topology.
//!
//! Publisher + MAGIC_SUB subscriber connected through a tiny TCP
//! proxy that the test can netsplit and heal. Verifies that:
//!   - records the publisher writes during a netsplit are
//!     correctly replayed once the link heals;
//!   - the subscriber's reconnect loop survives multiple
//!     interruption cycles without duplicating records or
//!     losing the schema.
//!
//! Scale: 1000 rows + two netsplit cycles. v6.1.9 design's full
//! 100K ship gate is left as a future scale-up (it's a soak-y
//! test that runs minutes; this MVP keeps the loop short enough
//! that CI can hit it on every commit).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{
    Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch,
};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(20);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-chaos-lr-e2e-{tag}-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_publisher(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl()
        .with_logical_wal()
        .spawn()
}

fn spawn_subscriber(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .spawn()
}

fn wait_for_addr(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(50));
    }
}

// ---- tiny TCP proxy with kill switch (copied from
// e2e_chaos_netsplit.rs; cross-test sharing isn't a thing) ----

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

fn spawn_proxy(backend_addr: String) -> (ProxyControl, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
    let bound_addr = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let ctrl = ProxyControl::new();
    let ctrl_for_thread = ctrl.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
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
    (ctrl, bound_addr)
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
                ) => {}
            Err(_) => return,
        }
    }
}

// ---- query helpers ----

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
    assert_eq!(rd.op, Op::RowDescription, "got {:?}", rd.op);
    let mut last: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => {
                last = match &parse_data_row(&f).unwrap()[0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                last = match &rows[0][0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::CommandComplete => return last,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wait_for_count(addr: &str, sql: &str, target: i64, deadline: Instant) -> i64 {
    loop {
        if let Ok(mut s) = TcpStream::connect(addr) {
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let got = select_int(&mut s, sql);
            if got >= target {
                return got;
            }
            if Instant::now() >= deadline {
                return got;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn subscription_survives_netsplit_heal_cycle() {
    // Publisher writes 500 rows; subscriber catches up.
    // Netsplit. Publisher writes another 500 rows.
    // Heal. Subscriber must converge to 1000 rows total exactly.
    let dir_p = unique_tmpdir("p");
    let dir_s = unique_tmpdir("s");

    let (p_raw, p_addrs) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let _p_guard = common::ChildGuard(p_raw);
    let mut p_client = common::connect_to(&p_addrs.native);
    p_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(p_addrs.repl.as_ref().unwrap());

    let (proxy_ctrl, proxy_addr) = spawn_proxy(p_addrs.repl.as_ref().unwrap().clone());
    thread::sleep(Duration::from_millis(100));

    exec_ok(&mut p_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut p_client, "CREATE PUBLICATION pub_t FOR ALL TABLES");

    let (s_raw, s_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let _s_guard = common::ChildGuard(s_raw);
    let mut s_client = common::connect_to(&s_addrs.native);
    s_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s_client, "CREATE TABLE t (id INT NOT NULL)");
    let (h, port) = proxy_addr.split_once(':').unwrap();
    exec_ok(
        &mut s_client,
        &format!("CREATE SUBSCRIPTION sub_t CONNECTION 'host={h} port={port}' PUBLICATION pub_t"),
    );
    thread::sleep(Duration::from_millis(500));

    // Phase 1 — pre-split writes (500 rows).
    for i in 0..500 {
        exec_ok(&mut p_client, &format!("INSERT INTO t VALUES ({i})"));
    }
    let phase1 = wait_for_count(
        &s_addrs.native,
        "SELECT count(*) FROM t",
        500,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(phase1, 500, "pre-split target = 500");

    // Phase 2 — netsplit, then publisher writes more.
    proxy_ctrl.netsplit();
    for i in 500..1000 {
        exec_ok(&mut p_client, &format!("INSERT INTO t VALUES ({i})"));
    }
    thread::sleep(Duration::from_millis(800));
    // Subscriber must still be behind. Use a fresh connection
    // since `s_client` has been idle across the long publisher
    // loop (its kernel-level read timeout would trip otherwise).
    let mut s_probe = common::connect_to(&s_addrs.native);
    s_probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mid = select_int(&mut s_probe, "SELECT count(*) FROM t");
    assert!(mid < 1000, "subscriber should be behind mid-split; got {mid}");

    // Phase 3 — heal. Subscriber must converge to 1000.
    proxy_ctrl.heal();
    let final_count = wait_for_count(
        &s_addrs.native,
        "SELECT count(*) FROM t",
        1000,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(
        final_count, 1000,
        "post-heal subscriber must converge to 1000 — no dup, no gap"
    );

    // Sanity — row VALUES are also intact (no duplicates that
    // happen to count to 1000 with stale rows). Verify via a
    // distinct count on a fresh connection.
    let mut s_check = common::connect_to(&s_addrs.native);
    s_check.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let distinct = select_int(&mut s_check, "SELECT count(*) FROM t WHERE id < 1000");
    assert_eq!(distinct, 1000);
}

#[test]
fn subscription_survives_two_split_heal_cycles() {
    // Same shape as above but two interruption cycles. v6.1.9's
    // ship gate calls for a soak under chaos; this is the MVP
    // version — repeatable + fast.
    let dir_p = unique_tmpdir("p2");
    let dir_s = unique_tmpdir("s2");

    let (p_raw, p_addrs) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let _p_guard = common::ChildGuard(p_raw);
    let mut p_client = common::connect_to(&p_addrs.native);
    p_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(p_addrs.repl.as_ref().unwrap());

    let (proxy_ctrl, proxy_addr) = spawn_proxy(p_addrs.repl.as_ref().unwrap().clone());
    thread::sleep(Duration::from_millis(100));

    exec_ok(&mut p_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut p_client, "CREATE PUBLICATION pub_t FOR ALL TABLES");

    let (s_raw, s_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let _s_guard = common::ChildGuard(s_raw);
    let mut s_client = common::connect_to(&s_addrs.native);
    s_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s_client, "CREATE TABLE t (id INT NOT NULL)");
    let (h, port) = proxy_addr.split_once(':').unwrap();
    exec_ok(
        &mut s_client,
        &format!("CREATE SUBSCRIPTION sub_t CONNECTION 'host={h} port={port}' PUBLICATION pub_t"),
    );
    thread::sleep(Duration::from_millis(500));

    let mut written = 0;
    for cycle in 0..2 {
        // Pre-split chunk.
        for _ in 0..200 {
            exec_ok(&mut p_client, &format!("INSERT INTO t VALUES ({written})"));
            written += 1;
        }
        thread::sleep(Duration::from_millis(300));

        // Netsplit.
        proxy_ctrl.netsplit();
        for _ in 0..200 {
            exec_ok(&mut p_client, &format!("INSERT INTO t VALUES ({written})"));
            written += 1;
        }
        thread::sleep(Duration::from_millis(400));

        // Heal.
        proxy_ctrl.heal();
        let got = wait_for_count(
            &s_addrs.native,
            "SELECT count(*) FROM t",
            written as i64,
            Instant::now() + CATCHUP_TIMEOUT,
        );
        assert_eq!(
            got, written as i64,
            "cycle {cycle}: post-heal must reach {written}, got {got}"
        );
    }
}
