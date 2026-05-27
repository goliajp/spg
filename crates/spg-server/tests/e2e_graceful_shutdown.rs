#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

//! v4.33 graceful shutdown — SIGTERM/SIGINT drains in-flight
//! connections up to `SPG_SHUTDOWN_DEADLINE_SEC`, refuses new
//! ones in the meantime, and exits 0.
//!
//! The test sends SIGTERM via `libc::kill` (vs `Child::kill` which
//! issues SIGKILL and would bypass the drain path entirely).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

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
    let p = std::env::temp_dir().join(format!("spg-shutdown-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(addr: &str, db: &Path, wal: &Path, env: &[(&str, String)]) -> Child {
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
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        // SIGKILL fallback for the case where the drain hung or the
        // test exits before the child finished its graceful path.
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
    if f.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

#[allow(unsafe_code)]
fn send_sigterm(child: &Child) {
    // SAFETY: `kill(2)` with a valid pid + signal number is the
    // standard POSIX termination primitive. The child is alive
    // (caller asserts this); SIGTERM is the systemd-style stop
    // signal that the server installs a handler for.
    let pid = i32::try_from(child.id()).expect("pid fits in i32");
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// SIGTERM → in-flight query completes, new connections refused
/// during/after the drain, child exits with status 0 within the
/// shutdown deadline.
#[test]
fn graceful_shutdown_drains_inflight_and_refuses_new_conns_and_exits_zero() {
    let addr = pick_free_addr();
    let dir = unique_tmpdir("drain");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let mut c = ChildGuard(spawn_server(
        &addr,
        &db,
        &wal,
        // Generous drain budget; test asserts the child exits well
        // before the budget so we know the drain logic — not the
        // deadline — released the process.
        &[("SPG_SHUTDOWN_DEADLINE_SEC", "10".to_string())],
    ));
    let mut bootstrap = wait_for_listener(&addr, &mut c.0);
    bootstrap.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut bootstrap, "CREATE TABLE g (id INT NOT NULL)");
    drop(bootstrap);

    // Open the conn that will be "in-flight" when SIGTERM lands.
    // The query is a recursive CTE chosen to take ~100-500ms so
    // SIGTERM has a real window to interrupt; it's also CPU-bound
    // so a non-trivial chunk of work is genuinely mid-flight.
    let mut inflight = TcpStream::connect(&addr).expect("inflight connect");
    inflight.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let slow_sql = "WITH RECURSIVE seq(n) AS (\
                    SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<20000\
                    ) SELECT count(*) FROM seq";
    send(&mut inflight, &build_query(slow_sql));
    // Yield briefly so the server thread has actually entered the
    // engine before SIGTERM lands.
    thread::sleep(Duration::from_millis(50));

    send_sigterm(&c.0);

    // 1. In-flight query must complete cleanly. Read frames until
    //    CommandComplete; any ErrorResponse fails the test.
    let mut saw_cc = false;
    for _ in 0..16 {
        let f = read_frame(&mut inflight);
        match f.op {
            Op::CommandComplete => {
                saw_cc = true;
                break;
            }
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("in-flight query got error during shutdown: {msg}");
            }
            // Pass-through: RowDescription / DataRow / DataRowBatch.
            _ => {}
        }
    }
    assert!(saw_cc, "in-flight query never returned CC");
    drop(inflight);

    // 2. New connections after SIGTERM must be refused. The TCP
    //    listener has stopped accepting; either the connect fails
    //    outright (ECONNREFUSED) or it succeeds at the OS layer
    //    but the server never reads the request and the socket
    //    closes when the process exits. Either is a "refusal".
    let refused = TcpStream::connect(&addr).is_err();
    let drained_by_exit = !refused && {
        // OS-level accept happened (kernel SYN backlog) but the
        // server isn't reading. Wait briefly; if the child exits
        // first the socket dies, which is also acceptable.
        thread::sleep(Duration::from_millis(100));
        c.0.try_wait().ok().flatten().is_some()
    };
    assert!(
        refused || drained_by_exit,
        "new connection wasn't refused and child still running"
    );

    // 3. Child exits with status 0, well within the 10s budget.
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(s) = c.0.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit within budget"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "expected exit 0 after graceful shutdown, got {status:?}"
    );
}
