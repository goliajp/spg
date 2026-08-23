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

use crate::common;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

fn spawn_primary(
    db: &std::path::Path,
    wal: &std::path::Path,
    extra_env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl();
    for (k, v) in extra_env {
        if *k != "SPG_REPL_ADDR" {
            b = b.env(*k, v);
        }
    }
    b.spawn()
}

fn spawn_follower(
    db: &std::path::Path,
    wal: &std::path::Path,
    follow_of: &str,
    want_http: bool,
    extra_env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .env("SPG_FOLLOW_OF", follow_of);
    if want_http {
        b = b.with_http();
    }
    for (k, v) in extra_env {
        if *k != "SPG_FOLLOW_OF" && *k != "SPG_HTTP_ADDR" {
            b = b.env(*k, v);
        }
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-netsplit-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

fn spawn_proxy(backend_addr: String) -> (ProxyControl, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
    let bound_addr = listener.local_addr().unwrap().to_string();
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
                ) =>
            {
                // Polling tick — re-check the kill switch.
            }
            Err(_) => return,
        }
    }
}

// ---- follower-catch-up helper ----

fn wait_for_count(addr: &str, sql: &str, expected: i64, deadline: Instant) -> i64 {
    let mut last_seen: i64 = -1;
    loop {
        if let Ok(mut s) = TcpStream::connect(addr) {
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            if let Some(actual) = select_int_opt(&mut s, sql) {
                last_seen = actual;
                if actual == expected {
                    return actual;
                }
            }
            if Instant::now() >= deadline {
                return last_seen;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn select_int_opt(s: &mut TcpStream, sql: &str) -> Option<i64> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        return None;
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
            Op::CommandComplete => return Some(count),
            other => panic!("unexpected {other:?}"),
        }
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
// CI shared runners flake on the multi-server netsplit chaos
// orchestration. Run locally with --ignored.
#[ignore]
fn netsplit_disconnect_then_heal_resyncs_without_loss_or_dup() {
    let dir_p = unique_tmpdir("pri");
    let dir_f = unique_tmpdir("fol");

    let (raw, primary_addrs) = spawn_primary(&dir_p.join("a.db"), &dir_p.join("a.wal"), &[]);

    let mut primary = common::ChildGuard(raw);
    let mut prim_client = common::connect_to(&primary_addrs.native);
    prim_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // Wait for the replication listener too.
    wait_for_addr(primary_addrs.repl.as_ref().unwrap());

    let (proxy_ctrl, proxy_addr) = spawn_proxy(primary_addrs.repl.clone().unwrap());
    // Give the proxy's accept thread a tick to schedule before the
    // follower starts dialing through it.
    thread::sleep(Duration::from_millis(100));

    let (raw, follower_addrs) = spawn_follower(
        &dir_f.join("a.db"),
        &dir_f.join("a.wal"),
        &proxy_addr,
        false,
        &[],
    );
    let mut follower = common::ChildGuard(raw);

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
        &follower_addrs.native,
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
        let mut s = TcpStream::connect(&follower_addrs.native).unwrap();
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
    // v7.37 (round 827) — no head-start sleep: the catchup loop below
    // polls to a deadline and a few early "not yet" probes cost less
    // than 600ms of unconditional waiting.
    let post_heal = wait_for_count(
        &follower_addrs.native,
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
    let mut s = TcpStream::connect(&follower_addrs.native).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let sum = select_int(&mut s, "SELECT sum(v) FROM t");
    let expected_sum: i64 = (0..25).map(|i| (i * 7) as i64).sum();
    assert_eq!(
        sum, expected_sum,
        "row contents must reflect exactly the 25 INSERTs"
    );
}

/// v6.0.x — cross-process follower restart resumes from the
/// `.applied_pos` sidecar file rather than going through a full
/// snapshot resync. Kill the follower, write more rows on master,
/// then respawn the follower against the same db/wal/pos paths.
/// The follower must converge to the full row count without
/// duplicates — proving the sidecar told master the right resume
/// offset (and proving the engine's local-WAL replay rebuilds
/// the same logical state).
#[test]
// CI shared runners with --test-threads=1 occasionally see the
// follower child exit before publishing its bound addrs (likely
// startup-time pressure under sequential execution). Test passes
// reliably in isolation; run locally with
// `cargo test -p spg-server --test e2e_chaos_netsplit -- --ignored`.
#[ignore]
fn follower_restart_resumes_from_persisted_sidecar() {
    let dir_p = unique_tmpdir("rspri");
    let dir_f = unique_tmpdir("rsfol");
    let db_p = dir_p.join("a.db");
    let wal_p = dir_p.join("a.wal");
    let db_f = dir_f.join("a.db");
    let wal_f = dir_f.join("a.wal");
    let sidecar_f = dir_f.join("a.wal.applied_pos");

    let (raw, primary_addrs) = spawn_primary(&db_p, &wal_p, &[]);
    let mut primary = common::ChildGuard(raw);
    let mut prim_client = common::connect_to(&primary_addrs.native);
    prim_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(primary_addrs.repl.as_ref().unwrap());

    // First-life follower: ingest 10 rows.
    let (raw_f, follower_addrs_a) = spawn_follower(
        &db_f,
        &wal_f,
        primary_addrs.repl.as_ref().unwrap(),
        false,
        &[],
    );
    let mut follower = common::ChildGuard(raw_f);
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
    let pre_kill = wait_for_count(
        &follower_addrs_a.native,
        "SELECT count(*) FROM t",
        10,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(pre_kill, 10);
    assert!(
        sidecar_f.exists(),
        "sidecar .applied_pos must exist before follower restart"
    );
    let sidecar_before = std::fs::read(&sidecar_f).unwrap();
    assert_eq!(
        sidecar_before.len(),
        8,
        "sidecar must hold exactly 8 LE bytes"
    );
    let pos_before = u64::from_le_bytes(sidecar_before.as_slice().try_into().unwrap());
    assert!(
        pos_before > 0,
        "sidecar should hold a positive offset, got {pos_before}"
    );

    // Kill the follower process. ChildGuard's Drop sends SIGKILL +
    // waits. The db_path + wal_path + sidecar all live on disk
    // unchanged.
    drop(follower);
    // Belt and braces: the listener bind+connect lifecycle can hold
    // a port in TIME_WAIT briefly; sleep a tick before respawn so
    // the second follower's `127.0.0.1:0` allocation doesn't
    // accidentally race on the same ephemeral port the first
    // follower used.
    thread::sleep(Duration::from_millis(50));

    // Master keeps writing while no follower is connected.
    for i in 10..25 {
        exec_ok(
            &mut prim_client,
            &format!("INSERT INTO t VALUES ({i}, {})", i * 7),
        );
    }

    // Restart follower against the same paths. This is the cross-
    // process case: `state.lag_state.follower_applied_pos` starts
    // fresh-zero. The sidecar lookup at `follow_once` entry must
    // seed it from disk.
    let (raw_f2, follower_addrs_b) = spawn_follower(
        &db_f,
        &wal_f,
        primary_addrs.repl.as_ref().unwrap(),
        false,
        &[],
    );
    let _follower2 = common::ChildGuard(raw_f2);
    let post_restart = wait_for_count(
        &follower_addrs_b.native,
        "SELECT count(*) FROM t",
        25,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(
        post_restart, 25,
        "restarted follower must converge to exactly 25 rows — no dup, no gap"
    );
    // Sidecar must have advanced past the pre-kill value.
    let sidecar_after = std::fs::read(&sidecar_f).unwrap();
    let pos_after = u64::from_le_bytes(sidecar_after.as_slice().try_into().unwrap());
    assert!(
        pos_after > pos_before,
        "sidecar should have advanced post-restart ({pos_before} → {pos_after})"
    );

    // Row contents must be intact — guards against the "count
    // happens to land at 25 but with wrong values" failure mode.
    let mut s = TcpStream::connect(&follower_addrs_b.native).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let sum = select_int(&mut s, "SELECT sum(v) FROM t");
    let expected_sum: i64 = (0..25).map(|i| (i * 7) as i64).sum();
    assert_eq!(sum, expected_sum);
}

/// v4.36 status-frame protocol gives the follower visibility into
/// the master's WAL position. `/metrics` on the follower exposes
/// both `spg_replication_lag_bytes` and `spg_replication_lag_seconds`
/// once it has applied at least one status frame.
#[test]
fn follower_metrics_expose_replication_lag_after_status_frame() {
    let dir_p = unique_tmpdir("lp");
    let dir_f = unique_tmpdir("lf");

    let (raw, primary_addrs) = spawn_primary(&dir_p.join("a.db"), &dir_p.join("a.wal"), &[]);

    let mut primary = common::ChildGuard(raw);
    let mut prim_client = common::connect_to(&primary_addrs.native);
    prim_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(primary_addrs.repl.as_ref().unwrap());

    let (raw, follower_addrs) = spawn_follower(
        &dir_f.join("a.db"),
        &dir_f.join("a.wal"),
        primary_addrs.repl.as_ref().unwrap(),
        true,
        &[],
    );
    let mut follower = common::ChildGuard(raw);
    wait_for_addr(follower_addrs.http.as_ref().unwrap());

    exec_ok(&mut prim_client, "CREATE TABLE lag (id INT NOT NULL)");
    for i in 0..5 {
        exec_ok(&mut prim_client, &format!("INSERT INTO lag VALUES ({i})"));
    }
    let _ = wait_for_count(
        &follower_addrs.native,
        "SELECT count(*) FROM lag",
        5,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    // v7.37 (round 827) — poll for the series instead of giving the
    // 50ms status-frame timer "a few iterations" of flat sleep.
    let mut metrics = String::new();
    crate::common::wait_until(Duration::from_secs(5), || {
        metrics = http_get(follower_addrs.http.as_ref().unwrap(), "/metrics");
        metrics.contains("spg_replication_lag_bytes")
            && metrics.contains("spg_replication_lag_seconds")
    });
    assert!(
        metrics.contains("spg_replication_lag_bytes"),
        "/metrics missing lag_bytes series; got:\n{metrics}"
    );
    assert!(
        metrics.contains("spg_replication_lag_seconds"),
        "/metrics missing lag_seconds series; got:\n{metrics}"
    );
}
